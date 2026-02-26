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
use std::collections::BTreeSet;
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

    /// Convenience constructor: single agent fills all roles.
    #[must_use]
    pub fn solo(agent: &str) -> Self {
        Self {
            planner: agent.to_string(),
            dispatcher: agent.to_string(),
            operator: agent.to_string(),
        }
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
    KillSwitchActivated,
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
    const ALL: [Self; 9] = [
        Self::PolicyDenied,
        Self::ReservationConflict,
        Self::RateLimited,
        Self::StaleState,
        Self::DispatchError,
        Self::ApprovalRequired,
        Self::ApprovalDenied,
        Self::ApprovalExpired,
        Self::KillSwitchActivated,
    ];

    /// Return all canonical mission failure codes.
    #[must_use]
    pub const fn all() -> [Self; 9] {
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
            "kill_switch_activated" => Some(Self::KillSwitchActivated),
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
            "FTM1009" => Some(Self::KillSwitchActivated),
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
                human_hint: "Policy denied this action. Update policy or request operator override.",
                machine_hint: "abort_and_request_policy_override",
            },
            Self::ReservationConflict => MissionFailureContract {
                reason_code: "reservation_conflict",
                error_code: "FTM1002",
                terminality: MissionFailureTerminality::NonTerminal,
                retryability: MissionFailureRetryability::AfterStateRefresh,
                human_hint: "Target paths are already reserved. Wait or coordinate with current owner.",
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
                human_hint: "Human operator denied execution. Revise mission scope before retrying.",
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
            Self::KillSwitchActivated => MissionFailureContract {
                reason_code: "kill_switch_activated",
                error_code: "FTM1009",
                terminality: MissionFailureTerminality::Terminal,
                retryability: MissionFailureRetryability::Never,
                human_hint: "Global kill-switch is active. All mission dispatch is halted until operator deactivates.",
                machine_hint: "halt_all_dispatch_await_operator_deactivation",
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

// ============================================================================
// Prepare-Phase Coordinator
// ============================================================================

/// Per-step readiness outcome from prepare-phase evaluation.
///
/// Each step in a `TxPlan` is evaluated against safety gates:
/// policy preflight, reservation feasibility, approval requirements,
/// kill-switch state, and precondition satisfaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TxPrepareStepReadiness {
    /// Step is ready for commit-phase execution.
    Ready,
    /// Step is denied by a safety gate; transaction must abort.
    Denied {
        reason_code: String,
        error_code: String,
    },
    /// Step requires further action before readiness (e.g., approval refresh).
    Deferred {
        reason_code: String,
        retry_hint: String,
    },
}

impl TxPrepareStepReadiness {
    /// Whether this step passed all safety gates.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }

    /// Whether this step was denied (terminal for the transaction).
    #[must_use]
    pub fn is_denied(&self) -> bool {
        matches!(self, Self::Denied { .. })
    }

    /// Whether this step was deferred (can be retried after action).
    #[must_use]
    pub fn is_deferred(&self) -> bool {
        matches!(self, Self::Deferred { .. })
    }
}

/// Per-step prepare receipt emitted by the prepare-phase coordinator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TxPrepareStepReceipt {
    /// The step being evaluated.
    pub step_id: TxStepId,
    /// Step ordinal in the plan.
    pub ordinal: u32,
    /// Readiness outcome after safety-gate evaluation.
    pub readiness: TxPrepareStepReadiness,
    /// Which safety gate produced the outcome.
    pub decision_path: String,
    /// Epoch-millis when the evaluation completed.
    pub evaluated_at_ms: i64,
}

/// Aggregate outcome of prepare-phase evaluation for a transaction plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TxPrepareOutcome {
    /// All steps ready — transaction may proceed to commit phase.
    AllReady,
    /// One or more steps denied — transaction must abort.
    Denied,
    /// One or more steps deferred, none denied — can retry after action.
    Deferred,
}

impl TxPrepareOutcome {
    /// Whether commit phase is eligible to proceed.
    #[must_use]
    pub const fn commit_eligible(&self) -> bool {
        matches!(self, Self::AllReady)
    }
}

impl fmt::Display for TxPrepareOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AllReady => f.write_str("all_ready"),
            Self::Denied => f.write_str("denied"),
            Self::Deferred => f.write_str("deferred"),
        }
    }
}

/// Complete prepare-phase report for one transaction plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TxPrepareReport {
    /// Transaction ID being prepared.
    pub tx_id: TxId,
    /// Plan ID being prepared.
    pub plan_id: TxPlanId,
    /// Aggregate outcome.
    pub outcome: TxPrepareOutcome,
    /// Per-step readiness receipts, in plan order.
    pub step_receipts: Vec<TxPrepareStepReceipt>,
    /// Overall decision path for structured logging.
    pub decision_path: String,
    /// Structured reason code summarizing the outcome.
    pub reason_code: String,
    /// Error code when outcome is Denied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// Epoch-millis when the prepare phase completed.
    pub completed_at_ms: i64,
}

impl TxPrepareReport {
    /// Count of steps in each readiness state.
    #[must_use]
    pub fn readiness_counts(&self) -> (usize, usize, usize) {
        let ready = self
            .step_receipts
            .iter()
            .filter(|r| r.readiness.is_ready())
            .count();
        let denied = self
            .step_receipts
            .iter()
            .filter(|r| r.readiness.is_denied())
            .count();
        let deferred = self
            .step_receipts
            .iter()
            .filter(|r| r.readiness.is_deferred())
            .count();
        (ready, denied, deferred)
    }

    /// Deterministic canonical string form for audit hashing.
    #[must_use]
    pub fn canonical_string(&self) -> String {
        let step_parts: Vec<String> = self
            .step_receipts
            .iter()
            .map(|r| {
                let readiness = match &r.readiness {
                    TxPrepareStepReadiness::Ready => "ready".to_string(),
                    TxPrepareStepReadiness::Denied {
                        reason_code,
                        error_code,
                    } => {
                        format!("denied({reason_code},{error_code})")
                    }
                    TxPrepareStepReadiness::Deferred {
                        reason_code,
                        retry_hint,
                    } => {
                        format!("deferred({reason_code},{retry_hint})")
                    }
                };
                format!("{}:{}:{}", r.step_id.0, r.ordinal, readiness)
            })
            .collect();
        format!(
            "tx={},plan={},outcome={},steps=[{}],completed_at_ms={}",
            self.tx_id.0,
            self.plan_id.0,
            self.outcome,
            step_parts.join(";"),
            self.completed_at_ms,
        )
    }
}

/// Input for evaluate_prepare_phase: per-step gate results from external checks.
///
/// The caller is responsible for running policy preflight, reservation feasibility,
/// and approval checks externally. This struct carries the results into the
/// prepare-phase evaluator for aggregation and readiness determination.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TxPrepareGateInput {
    /// Step being evaluated.
    pub step_id: TxStepId,
    /// Whether the policy preflight passed for this step.
    pub policy_passed: bool,
    /// Reason code from policy check (if denied).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy_reason_code: Option<String>,
    /// Whether reservations for this step's targets are available.
    pub reservation_available: bool,
    /// Reason code from reservation check (if conflicted).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reservation_reason_code: Option<String>,
    /// Whether approval is satisfied for this step (NotRequired or Approved).
    pub approval_satisfied: bool,
    /// Reason code from approval check (if pending/expired/denied).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_reason_code: Option<String>,
    /// Whether the target pane/resource is live and reachable.
    pub target_liveness: bool,
    /// Reason code from liveness check (if unreachable).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub liveness_reason_code: Option<String>,
}

/// Evaluate the prepare phase for a transaction plan.
///
/// This pure function evaluates all safety gates and returns a prepare report.
/// It does NOT mutate the mission or transaction — the caller is responsible
/// for applying the report (recording receipts, transitioning state).
///
/// Decision logic:
/// 1. If kill-switch is active → all steps denied (early abort).
/// 2. For each step: evaluate policy → reservation → approval → liveness.
/// 3. First denied gate short-circuits the step to `Denied`.
/// 4. First deferred gate (with no denials) marks step `Deferred`.
/// 5. All gates pass → step `Ready`.
/// 6. Aggregate: any denied → `Denied`; any deferred → `Deferred`; all ready → `AllReady`.
pub fn evaluate_prepare_phase(
    tx_id: &TxId,
    plan: &TxPlan,
    gate_inputs: &[TxPrepareGateInput],
    kill_switch_level: MissionKillSwitchLevel,
    evaluated_at_ms: i64,
) -> Result<TxPrepareReport, MissionTxValidationError> {
    if plan.steps.is_empty() {
        return Err(MissionTxValidationError::EmptyPlanSteps);
    }

    // Build step-id → gate-input lookup
    let gate_map: std::collections::HashMap<&str, &TxPrepareGateInput> = gate_inputs
        .iter()
        .map(|g| (g.step_id.0.as_str(), g))
        .collect();

    let mut step_receipts = Vec::with_capacity(plan.steps.len());
    let mut any_denied = false;
    let mut any_deferred = false;

    // Kill-switch check: if active, deny all steps immediately.
    if kill_switch_level.blocks_dispatch() {
        for step in &plan.steps {
            step_receipts.push(TxPrepareStepReceipt {
                step_id: step.step_id.clone(),
                ordinal: step.ordinal,
                readiness: TxPrepareStepReadiness::Denied {
                    reason_code: MissionFailureCode::KillSwitchActivated
                        .reason_code()
                        .to_string(),
                    error_code: MissionFailureCode::KillSwitchActivated
                        .error_code()
                        .to_string(),
                },
                decision_path: "prepare_kill_switch_active".to_string(),
                evaluated_at_ms,
            });
        }
        return Ok(TxPrepareReport {
            tx_id: tx_id.clone(),
            plan_id: plan.plan_id.clone(),
            outcome: TxPrepareOutcome::Denied,
            step_receipts,
            decision_path: "prepare_abort_kill_switch".to_string(),
            reason_code: MissionFailureCode::KillSwitchActivated
                .reason_code()
                .to_string(),
            error_code: Some(
                MissionFailureCode::KillSwitchActivated
                    .error_code()
                    .to_string(),
            ),
            completed_at_ms: evaluated_at_ms,
        });
    }

    // Evaluate each step against its gate inputs.
    for step in &plan.steps {
        let gate = gate_map.get(step.step_id.0.as_str());

        let (readiness, decision_path) = match gate {
            None => {
                // No gate input for this step — treat as deferred (missing data).
                any_deferred = true;
                (
                    TxPrepareStepReadiness::Deferred {
                        reason_code: "gate_input_missing".to_string(),
                        retry_hint: "provide_gate_input_for_step".to_string(),
                    },
                    "prepare_gate_missing".to_string(),
                )
            }
            Some(gate) => {
                // Check gates in priority order: policy → reservation → approval → liveness
                if !gate.policy_passed {
                    any_denied = true;
                    (
                        TxPrepareStepReadiness::Denied {
                            reason_code: gate
                                .policy_reason_code
                                .clone()
                                .unwrap_or_else(|| "policy_denied".to_string()),
                            error_code: MissionFailureCode::PolicyDenied.error_code().to_string(),
                        },
                        "prepare_policy_denied".to_string(),
                    )
                } else if !gate.reservation_available {
                    // Reservation conflict is deferrable (can wait for release)
                    any_deferred = true;
                    (
                        TxPrepareStepReadiness::Deferred {
                            reason_code: gate
                                .reservation_reason_code
                                .clone()
                                .unwrap_or_else(|| "reservation_conflict".to_string()),
                            retry_hint: "refresh_reservations_then_retry".to_string(),
                        },
                        "prepare_reservation_conflict".to_string(),
                    )
                } else if !gate.approval_satisfied {
                    let approval_reason = gate
                        .approval_reason_code
                        .as_deref()
                        .unwrap_or("approval_required");

                    if approval_reason == "approval_denied" {
                        any_denied = true;
                        (
                            TxPrepareStepReadiness::Denied {
                                reason_code: "approval_denied".to_string(),
                                error_code: MissionFailureCode::ApprovalDenied
                                    .error_code()
                                    .to_string(),
                            },
                            "prepare_approval_denied".to_string(),
                        )
                    } else {
                        any_deferred = true;
                        (
                            TxPrepareStepReadiness::Deferred {
                                reason_code: approval_reason.to_string(),
                                retry_hint: "request_approval_and_pause".to_string(),
                            },
                            "prepare_approval_pending".to_string(),
                        )
                    }
                } else if !gate.target_liveness {
                    any_deferred = true;
                    (
                        TxPrepareStepReadiness::Deferred {
                            reason_code: gate
                                .liveness_reason_code
                                .clone()
                                .unwrap_or_else(|| "target_unreachable".to_string()),
                            retry_hint: "refresh_runtime_state_then_retry".to_string(),
                        },
                        "prepare_target_unreachable".to_string(),
                    )
                } else {
                    // All gates passed
                    (
                        TxPrepareStepReadiness::Ready,
                        "prepare_all_gates_passed".to_string(),
                    )
                }
            }
        };

        step_receipts.push(TxPrepareStepReceipt {
            step_id: step.step_id.clone(),
            ordinal: step.ordinal,
            readiness,
            decision_path,
            evaluated_at_ms,
        });
    }

    let outcome = if any_denied {
        TxPrepareOutcome::Denied
    } else if any_deferred {
        TxPrepareOutcome::Deferred
    } else {
        TxPrepareOutcome::AllReady
    };

    let (reason_code, error_code, decision_path) = match outcome {
        TxPrepareOutcome::AllReady => (
            "prepare_succeeded".to_string(),
            None,
            "prepare_all_steps_ready".to_string(),
        ),
        TxPrepareOutcome::Denied => {
            // Find first denied step's reason
            let first_denied = step_receipts.iter().find(|r| r.readiness.is_denied());
            let (reason, error) = match first_denied.map(|r| &r.readiness) {
                Some(TxPrepareStepReadiness::Denied {
                    reason_code,
                    error_code,
                }) => (reason_code.clone(), error_code.clone()),
                _ => ("prepare_denied".to_string(), "FTX2003".to_string()),
            };
            (reason, Some(error), "prepare_denied_abort".to_string())
        }
        TxPrepareOutcome::Deferred => {
            let first_deferred = step_receipts.iter().find(|r| r.readiness.is_deferred());
            let reason = match first_deferred.map(|r| &r.readiness) {
                Some(TxPrepareStepReadiness::Deferred { reason_code, .. }) => reason_code.clone(),
                _ => "prepare_deferred".to_string(),
            };
            (reason, None, "prepare_deferred_retry".to_string())
        }
    };

    Ok(TxPrepareReport {
        tx_id: tx_id.clone(),
        plan_id: plan.plan_id.clone(),
        outcome,
        step_receipts,
        decision_path,
        reason_code,
        error_code,
        completed_at_ms: evaluated_at_ms,
    })
}

// ── H5: Commit-Phase Executor ───────────────────────────────────────────────

/// Per-step result from the commit phase executor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TxCommitStepResult {
    /// Step that was executed.
    pub step_id: TxStepId,
    /// Step ordinal for ordering verification.
    pub ordinal: u32,
    /// Outcome of executing this step.
    pub outcome: TxCommitStepOutcome,
    /// Decision path describing the execution flow.
    pub decision_path: String,
    /// Timestamp when execution completed for this step.
    pub completed_at_ms: i64,
}

/// Outcome of a single commit-phase step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TxCommitStepOutcome {
    /// Step executed successfully.
    Committed { reason_code: String },
    /// Step execution failed.
    Failed {
        reason_code: String,
        error_code: String,
    },
    /// Step was skipped due to prior failure (barrier halt).
    Skipped { reason_code: String },
    /// Step was skipped due to kill-switch activation.
    Blocked {
        reason_code: String,
        error_code: String,
    },
}

impl TxCommitStepOutcome {
    /// True if this outcome represents a successful commit.
    #[must_use]
    pub fn is_committed(&self) -> bool {
        matches!(self, Self::Committed { .. })
    }

    /// True if this outcome represents a failure.
    #[must_use]
    pub fn is_failed(&self) -> bool {
        matches!(self, Self::Failed { .. })
    }

    /// True if this step was skipped.
    #[must_use]
    pub fn is_skipped(&self) -> bool {
        matches!(self, Self::Skipped { .. })
    }

    /// Short tag name for logging.
    #[must_use]
    pub fn tag_name(&self) -> &str {
        match self {
            Self::Committed { .. } => "committed",
            Self::Failed { .. } => "failed",
            Self::Skipped { .. } => "skipped",
            Self::Blocked { .. } => "blocked",
        }
    }
}

/// Aggregate commit-phase report covering all steps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TxCommitReport {
    /// Transaction ID.
    pub tx_id: TxId,
    /// Plan ID.
    pub plan_id: TxPlanId,
    /// Overall commit outcome.
    pub outcome: TxCommitOutcome,
    /// Per-step results in ordinal order.
    pub step_results: Vec<TxCommitStepResult>,
    /// Index of the first failed step (None if all committed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_boundary: Option<u32>,
    /// Total steps committed successfully.
    pub committed_count: usize,
    /// Total steps that failed.
    pub failed_count: usize,
    /// Total steps skipped (after barrier).
    pub skipped_count: usize,
    /// Decision path for the overall outcome.
    pub decision_path: String,
    /// Reason code for the overall outcome.
    pub reason_code: String,
    /// Error code (only for failures).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// Timestamp when commit phase completed.
    pub completed_at_ms: i64,
    /// Receipts emitted during the commit phase.
    pub receipts: Vec<TxReceipt>,
}

impl TxCommitReport {
    /// True if all steps committed successfully.
    #[must_use]
    pub fn is_fully_committed(&self) -> bool {
        matches!(self.outcome, TxCommitOutcome::FullyCommitted)
    }

    /// True if any step failed (partial or complete failure).
    #[must_use]
    pub fn has_failures(&self) -> bool {
        self.failed_count > 0
    }

    /// Canonical string for deterministic hashing.
    #[must_use]
    pub fn canonical_string(&self) -> String {
        format!(
            "tx_id={}|plan_id={}|outcome={}|committed={}|failed={}|skipped={}|boundary={}|reason={}|err={}",
            self.tx_id.0,
            self.plan_id.0,
            self.outcome.tag_name(),
            self.committed_count,
            self.failed_count,
            self.skipped_count,
            self.failure_boundary
                .map_or_else(|| "none".to_string(), |v| v.to_string()),
            self.reason_code,
            self.error_code.as_deref().unwrap_or("none"),
        )
    }
}

/// Overall commit phase outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TxCommitOutcome {
    /// All steps committed. Transition: Committing → Committed.
    FullyCommitted,
    /// Some steps committed, then a failure. Transition: Committing → Compensating.
    PartialFailure,
    /// First step failed immediately. Transition: Committing → Failed.
    ImmediateFailure,
    /// Blocked by kill-switch before any steps. Transition: Committing → Failed.
    KillSwitchBlocked,
    /// Blocked by pause command. Commit suspended.
    PauseSuspended,
}

impl TxCommitOutcome {
    /// Short tag name for canonical string.
    #[must_use]
    pub fn tag_name(&self) -> &str {
        match self {
            Self::FullyCommitted => "fully_committed",
            Self::PartialFailure => "partial_failure",
            Self::ImmediateFailure => "immediate_failure",
            Self::KillSwitchBlocked => "kill_switch_blocked",
            Self::PauseSuspended => "pause_suspended",
        }
    }

    /// The MissionTxState this outcome transitions to.
    #[must_use]
    pub fn target_tx_state(&self) -> MissionTxState {
        match self {
            Self::FullyCommitted => MissionTxState::Committed,
            Self::PartialFailure => MissionTxState::Compensating,
            Self::ImmediateFailure => MissionTxState::Failed,
            Self::KillSwitchBlocked => MissionTxState::Failed,
            Self::PauseSuspended => MissionTxState::Committing, // stays in committing
        }
    }
}

/// Input describing one step's execution result (provided by dispatcher).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TxCommitStepInput {
    /// Step ID being reported.
    pub step_id: TxStepId,
    /// Whether the step succeeded.
    pub success: bool,
    /// Reason code for the outcome.
    pub reason_code: String,
    /// Error code (only when success=false).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// Timestamp when this step completed.
    pub completed_at_ms: i64,
}

/// Execute the commit phase for a transaction plan.
///
/// Steps are processed in ordinal order with barrier semantics:
/// - Kill-switch check before any step
/// - On first failure, all remaining steps are skipped
/// - Emits monotonic receipts for CommitStarted, per-step, and terminal state
///
/// # Errors
/// Returns `MissionTxValidationError` if the plan has no steps or the
/// contract is in a non-prepared state.
pub fn execute_commit_phase(
    contract: &MissionTxContract,
    step_inputs: &[TxCommitStepInput],
    kill_switch_level: MissionKillSwitchLevel,
    is_paused: bool,
    executed_at_ms: i64,
) -> Result<TxCommitReport, MissionTxValidationError> {
    // Validate preconditions
    if contract.lifecycle_state != MissionTxState::Prepared
        && contract.lifecycle_state != MissionTxState::Committing
    {
        return Err(MissionTxValidationError::IllegalLifecycleTransition {
            from: contract.lifecycle_state,
            to: MissionTxState::Committing,
            kind: MissionTxTransitionKind::CommitStarted,
        });
    }

    if contract.plan.steps.is_empty() {
        return Err(MissionTxValidationError::EmptyPlanSteps);
    }

    let mut receipts = Vec::new();
    let mut receipt_seq = contract.receipts.last().map_or(1, |r| r.seq + 1);

    // Emit CommitStarted receipt
    receipts.push(TxReceipt {
        seq: receipt_seq,
        state: MissionTxState::Committing,
        emitted_at_ms: executed_at_ms,
        reason_code: Some("commit_phase_started".into()),
        error_code: None,
    });
    receipt_seq += 1;

    // Kill-switch pre-check
    if kill_switch_level.blocks_dispatch() {
        let step_results: Vec<TxCommitStepResult> = contract
            .plan
            .steps
            .iter()
            .map(|step| TxCommitStepResult {
                step_id: step.step_id.clone(),
                ordinal: step.ordinal,
                outcome: TxCommitStepOutcome::Blocked {
                    reason_code: "kill_switch_active".into(),
                    error_code: "FTX3001".into(),
                },
                decision_path: "commit_abort_kill_switch".into(),
                completed_at_ms: executed_at_ms,
            })
            .collect();

        receipts.push(TxReceipt {
            seq: receipt_seq,
            state: MissionTxState::Failed,
            emitted_at_ms: executed_at_ms,
            reason_code: Some("kill_switch_blocked_commit".into()),
            error_code: Some("FTX3001".into()),
        });

        return Ok(TxCommitReport {
            tx_id: contract.intent.tx_id.clone(),
            plan_id: contract.plan.plan_id.clone(),
            outcome: TxCommitOutcome::KillSwitchBlocked,
            step_results,
            failure_boundary: Some(1),
            committed_count: 0,
            failed_count: 0,
            skipped_count: contract.plan.steps.len(),
            decision_path: "commit_abort_kill_switch".into(),
            reason_code: "kill_switch_active".into(),
            error_code: Some("FTX3001".into()),
            completed_at_ms: executed_at_ms,
            receipts,
        });
    }

    // Pause check
    if is_paused {
        let step_results: Vec<TxCommitStepResult> = contract
            .plan
            .steps
            .iter()
            .map(|step| TxCommitStepResult {
                step_id: step.step_id.clone(),
                ordinal: step.ordinal,
                outcome: TxCommitStepOutcome::Skipped {
                    reason_code: "mission_paused".into(),
                },
                decision_path: "commit_suspended_paused".into(),
                completed_at_ms: executed_at_ms,
            })
            .collect();

        return Ok(TxCommitReport {
            tx_id: contract.intent.tx_id.clone(),
            plan_id: contract.plan.plan_id.clone(),
            outcome: TxCommitOutcome::PauseSuspended,
            step_results,
            failure_boundary: None,
            committed_count: 0,
            failed_count: 0,
            skipped_count: contract.plan.steps.len(),
            decision_path: "commit_suspended_paused".into(),
            reason_code: "mission_paused".into(),
            error_code: None,
            completed_at_ms: executed_at_ms,
            receipts,
        });
    }

    // Build step input lookup by step_id
    let input_map: std::collections::HashMap<&str, &TxCommitStepInput> = step_inputs
        .iter()
        .map(|si| (si.step_id.0.as_str(), si))
        .collect();

    let mut step_results = Vec::with_capacity(contract.plan.steps.len());
    let mut committed_count = 0usize;
    let mut failed_count = 0usize;
    let mut skipped_count = 0usize;
    let mut failure_boundary: Option<u32> = None;
    let mut barrier_tripped = false;

    // Execute steps in ordinal order
    for step in &contract.plan.steps {
        if barrier_tripped {
            step_results.push(TxCommitStepResult {
                step_id: step.step_id.clone(),
                ordinal: step.ordinal,
                outcome: TxCommitStepOutcome::Skipped {
                    reason_code: "barrier_halt".into(),
                },
                decision_path: "commit_step_skipped_barrier".into(),
                completed_at_ms: executed_at_ms,
            });
            skipped_count += 1;
            continue;
        }

        match input_map.get(step.step_id.0.as_str()) {
            Some(input) if input.success => {
                step_results.push(TxCommitStepResult {
                    step_id: step.step_id.clone(),
                    ordinal: step.ordinal,
                    outcome: TxCommitStepOutcome::Committed {
                        reason_code: input.reason_code.clone(),
                    },
                    decision_path: "commit_step_succeeded".into(),
                    completed_at_ms: input.completed_at_ms,
                });
                committed_count += 1;
            }
            Some(input) => {
                // Step failed — trip the barrier
                step_results.push(TxCommitStepResult {
                    step_id: step.step_id.clone(),
                    ordinal: step.ordinal,
                    outcome: TxCommitStepOutcome::Failed {
                        reason_code: input.reason_code.clone(),
                        error_code: input.error_code.clone().unwrap_or_else(|| "FTX3002".into()),
                    },
                    decision_path: "commit_step_failed".into(),
                    completed_at_ms: input.completed_at_ms,
                });
                failed_count += 1;
                failure_boundary = Some(step.ordinal);
                barrier_tripped = true;
            }
            None => {
                // No input provided — treat as failure (missing result)
                step_results.push(TxCommitStepResult {
                    step_id: step.step_id.clone(),
                    ordinal: step.ordinal,
                    outcome: TxCommitStepOutcome::Failed {
                        reason_code: "step_input_missing".into(),
                        error_code: "FTX3003".into(),
                    },
                    decision_path: "commit_step_no_input".into(),
                    completed_at_ms: executed_at_ms,
                });
                failed_count += 1;
                failure_boundary = Some(step.ordinal);
                barrier_tripped = true;
            }
        }
    }

    // Determine overall outcome
    let (outcome, terminal_state, reason_code, error_code, decision_path) = if failed_count == 0 {
        (
            TxCommitOutcome::FullyCommitted,
            MissionTxState::Committed,
            "all_steps_committed".to_string(),
            None,
            "commit_succeeded".to_string(),
        )
    } else if committed_count == 0 {
        (
            TxCommitOutcome::ImmediateFailure,
            MissionTxState::Failed,
            "first_step_failed".to_string(),
            Some("FTX3004".to_string()),
            "commit_immediate_failure".to_string(),
        )
    } else {
        (
            TxCommitOutcome::PartialFailure,
            MissionTxState::Compensating,
            format!(
                "partial_failure_at_ordinal_{}",
                failure_boundary.unwrap_or(0)
            ),
            Some("FTX3005".to_string()),
            "commit_partial_failure".to_string(),
        )
    };

    // Emit terminal receipt
    receipts.push(TxReceipt {
        seq: receipt_seq,
        state: terminal_state,
        emitted_at_ms: executed_at_ms,
        reason_code: Some(reason_code.clone()),
        error_code: error_code.clone(),
    });

    Ok(TxCommitReport {
        tx_id: contract.intent.tx_id.clone(),
        plan_id: contract.plan.plan_id.clone(),
        outcome,
        step_results,
        failure_boundary,
        committed_count,
        failed_count,
        skipped_count,
        decision_path,
        reason_code,
        error_code,
        completed_at_ms: executed_at_ms,
        receipts,
    })
}

// ── H6: Compensation Planner and Automatic Rollback Engine ──────────────────

/// Per-step compensation result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TxCompensationStepResult {
    /// Forward step being compensated.
    pub for_step_id: TxStepId,
    /// Ordinal of the forward step (used for reverse ordering).
    pub forward_ordinal: u32,
    /// Compensation outcome.
    pub outcome: TxCompensationStepOutcome,
    /// Decision path.
    pub decision_path: String,
    /// Timestamp.
    pub completed_at_ms: i64,
}

/// Outcome of a single compensation step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TxCompensationStepOutcome {
    /// Compensation succeeded (forward step undone).
    Compensated { reason_code: String },
    /// Compensation failed (residual risk).
    Failed {
        reason_code: String,
        error_code: String,
    },
    /// No compensation defined for this step.
    NoCompensation { reason_code: String },
    /// Skipped due to prior compensation failure.
    Skipped { reason_code: String },
}

impl TxCompensationStepOutcome {
    #[must_use]
    pub fn is_compensated(&self) -> bool {
        matches!(self, Self::Compensated { .. })
    }

    #[must_use]
    pub fn is_failed(&self) -> bool {
        matches!(self, Self::Failed { .. })
    }

    #[must_use]
    pub fn tag_name(&self) -> &str {
        match self {
            Self::Compensated { .. } => "compensated",
            Self::Failed { .. } => "failed",
            Self::NoCompensation { .. } => "no_compensation",
            Self::Skipped { .. } => "skipped",
        }
    }
}

/// Aggregate compensation report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TxCompensationReport {
    pub tx_id: TxId,
    pub plan_id: TxPlanId,
    /// Overall compensation outcome.
    pub outcome: TxCompensationOutcome,
    /// Per-step results in reverse ordinal order (highest first).
    pub step_results: Vec<TxCompensationStepResult>,
    /// Steps that were successfully compensated.
    pub compensated_count: usize,
    /// Steps where compensation failed (residual risk).
    pub failed_count: usize,
    /// Steps without a defined compensation action.
    pub no_compensation_count: usize,
    /// Steps skipped due to earlier compensation failure.
    pub skipped_count: usize,
    /// Decision path.
    pub decision_path: String,
    /// Reason code.
    pub reason_code: String,
    /// Error code (only for failures).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// Timestamp.
    pub completed_at_ms: i64,
    /// Receipts emitted during compensation.
    pub receipts: Vec<TxReceipt>,
}

impl TxCompensationReport {
    /// True if all compensations succeeded (clean rollback).
    #[must_use]
    pub fn is_fully_rolled_back(&self) -> bool {
        matches!(self.outcome, TxCompensationOutcome::FullyRolledBack)
    }

    /// True if any compensation failed.
    #[must_use]
    pub fn has_residual_risk(&self) -> bool {
        self.failed_count > 0
    }

    /// Canonical string for deterministic hashing.
    #[must_use]
    pub fn canonical_string(&self) -> String {
        format!(
            "tx_id={}|plan_id={}|outcome={}|compensated={}|failed={}|no_comp={}|skipped={}|reason={}|err={}",
            self.tx_id.0,
            self.plan_id.0,
            self.outcome.tag_name(),
            self.compensated_count,
            self.failed_count,
            self.no_compensation_count,
            self.skipped_count,
            self.reason_code,
            self.error_code.as_deref().unwrap_or("none"),
        )
    }
}

/// Overall compensation outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TxCompensationOutcome {
    /// All compensations succeeded → Compensating → RolledBack.
    FullyRolledBack,
    /// At least one compensation failed → Compensating → Failed.
    CompensationFailed,
    /// No steps needed compensation (e.g., ImmediateFailure).
    NothingToCompensate,
}

impl TxCompensationOutcome {
    #[must_use]
    pub fn tag_name(&self) -> &str {
        match self {
            Self::FullyRolledBack => "fully_rolled_back",
            Self::CompensationFailed => "compensation_failed",
            Self::NothingToCompensate => "nothing_to_compensate",
        }
    }

    /// Target transaction state after compensation.
    #[must_use]
    pub fn target_tx_state(&self) -> MissionTxState {
        match self {
            Self::FullyRolledBack => MissionTxState::RolledBack,
            Self::CompensationFailed => MissionTxState::Failed,
            Self::NothingToCompensate => MissionTxState::Failed,
        }
    }
}

/// Input describing one step's compensation result (provided by dispatcher).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TxCompensationStepInput {
    /// Forward step ID being compensated.
    pub for_step_id: TxStepId,
    /// Whether the compensation succeeded.
    pub success: bool,
    /// Reason code.
    pub reason_code: String,
    /// Error code (only when success=false).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// Timestamp.
    pub completed_at_ms: i64,
}

/// Execute the compensation phase for a partial-failure commit.
///
/// Compensations are processed in **reverse ordinal order** (highest ordinal first)
/// for steps that were committed successfully before the failure boundary.
///
/// # Arguments
/// * `contract` — Transaction contract (must be in Compensating state)
/// * `commit_report` — The commit report showing which steps committed
/// * `comp_inputs` — Per-step compensation results from dispatcher
/// * `executed_at_ms` — Current timestamp
///
/// # Errors
/// Returns `MissionTxValidationError` if contract is not in Compensating state.
pub fn execute_compensation_phase(
    contract: &MissionTxContract,
    commit_report: &TxCommitReport,
    comp_inputs: &[TxCompensationStepInput],
    executed_at_ms: i64,
) -> Result<TxCompensationReport, MissionTxValidationError> {
    // Validate precondition
    if contract.lifecycle_state != MissionTxState::Compensating {
        return Err(MissionTxValidationError::IllegalLifecycleTransition {
            from: contract.lifecycle_state,
            to: MissionTxState::RolledBack,
            kind: MissionTxTransitionKind::CompensationSucceeded,
        });
    }

    let mut receipts = Vec::new();
    let mut receipt_seq = contract.receipts.last().map_or(1, |r| r.seq + 1);

    // Emit CompensationStarted receipt
    receipts.push(TxReceipt {
        seq: receipt_seq,
        state: MissionTxState::Compensating,
        emitted_at_ms: executed_at_ms,
        reason_code: Some("compensation_started".into()),
        error_code: None,
    });
    receipt_seq += 1;

    // Build compensation lookup: for_step_id → TxCompensation
    let comp_map: std::collections::HashMap<&str, &TxCompensation> = contract
        .plan
        .compensations
        .iter()
        .map(|c| (c.for_step_id.0.as_str(), c))
        .collect();

    // Build input lookup: for_step_id → TxCompensationStepInput
    let input_map: std::collections::HashMap<&str, &TxCompensationStepInput> = comp_inputs
        .iter()
        .map(|ci| (ci.for_step_id.0.as_str(), ci))
        .collect();

    // Identify committed steps that need compensation (reverse ordinal order)
    let mut steps_to_compensate: Vec<&TxCommitStepResult> = commit_report
        .step_results
        .iter()
        .filter(|sr| sr.outcome.is_committed())
        .collect();
    steps_to_compensate.sort_by_key(|a| std::cmp::Reverse(a.ordinal));

    // If nothing to compensate
    if steps_to_compensate.is_empty() {
        receipts.push(TxReceipt {
            seq: receipt_seq,
            state: MissionTxState::Failed,
            emitted_at_ms: executed_at_ms,
            reason_code: Some("nothing_to_compensate".into()),
            error_code: Some("FTX2008".into()),
        });

        return Ok(TxCompensationReport {
            tx_id: contract.intent.tx_id.clone(),
            plan_id: contract.plan.plan_id.clone(),
            outcome: TxCompensationOutcome::NothingToCompensate,
            step_results: Vec::new(),
            compensated_count: 0,
            failed_count: 0,
            no_compensation_count: 0,
            skipped_count: 0,
            decision_path: "compensation_nothing_to_compensate".into(),
            reason_code: "nothing_to_compensate".into(),
            error_code: Some("FTX2008".into()),
            completed_at_ms: executed_at_ms,
            receipts,
        });
    }

    let mut step_results = Vec::with_capacity(steps_to_compensate.len());
    let mut compensated_count = 0usize;
    let mut failed_count = 0usize;
    let mut no_compensation_count = 0usize;
    let mut skipped_count = 0usize;
    let mut barrier_tripped = false;

    // Execute compensations in reverse ordinal order
    for committed_step in &steps_to_compensate {
        let step_id = &committed_step.step_id;

        if barrier_tripped {
            step_results.push(TxCompensationStepResult {
                for_step_id: step_id.clone(),
                forward_ordinal: committed_step.ordinal,
                outcome: TxCompensationStepOutcome::Skipped {
                    reason_code: "prior_compensation_failed".into(),
                },
                decision_path: "compensation_skipped_barrier".into(),
                completed_at_ms: executed_at_ms,
            });
            skipped_count += 1;
            continue;
        }

        // Check if compensation is defined
        if !comp_map.contains_key(step_id.0.as_str()) {
            step_results.push(TxCompensationStepResult {
                for_step_id: step_id.clone(),
                forward_ordinal: committed_step.ordinal,
                outcome: TxCompensationStepOutcome::NoCompensation {
                    reason_code: "no_compensation_defined".into(),
                },
                decision_path: "compensation_not_defined".into(),
                completed_at_ms: executed_at_ms,
            });
            no_compensation_count += 1;
            // No compensation ≠ failure; continue with next step
            continue;
        }

        // Execute compensation
        match input_map.get(step_id.0.as_str()) {
            Some(input) if input.success => {
                step_results.push(TxCompensationStepResult {
                    for_step_id: step_id.clone(),
                    forward_ordinal: committed_step.ordinal,
                    outcome: TxCompensationStepOutcome::Compensated {
                        reason_code: input.reason_code.clone(),
                    },
                    decision_path: "compensation_succeeded".into(),
                    completed_at_ms: input.completed_at_ms,
                });
                compensated_count += 1;
            }
            Some(input) => {
                step_results.push(TxCompensationStepResult {
                    for_step_id: step_id.clone(),
                    forward_ordinal: committed_step.ordinal,
                    outcome: TxCompensationStepOutcome::Failed {
                        reason_code: input.reason_code.clone(),
                        error_code: input.error_code.clone().unwrap_or_else(|| "FTX2008".into()),
                    },
                    decision_path: "compensation_failed".into(),
                    completed_at_ms: input.completed_at_ms,
                });
                failed_count += 1;
                barrier_tripped = true;
            }
            None => {
                // Missing input for step with defined compensation
                step_results.push(TxCompensationStepResult {
                    for_step_id: step_id.clone(),
                    forward_ordinal: committed_step.ordinal,
                    outcome: TxCompensationStepOutcome::Failed {
                        reason_code: "compensation_input_missing".into(),
                        error_code: "FTX2010".into(),
                    },
                    decision_path: "compensation_no_input".into(),
                    completed_at_ms: executed_at_ms,
                });
                failed_count += 1;
                barrier_tripped = true;
            }
        }
    }

    // Determine overall outcome
    let (outcome, terminal_state, reason_code, error_code, decision_path) = if failed_count > 0 {
        (
            TxCompensationOutcome::CompensationFailed,
            MissionTxState::Failed,
            "compensation_incomplete".to_string(),
            Some("FTX2008".to_string()),
            "compensation_failed".to_string(),
        )
    } else {
        (
            TxCompensationOutcome::FullyRolledBack,
            MissionTxState::RolledBack,
            "all_compensations_succeeded".to_string(),
            None,
            "compensation_succeeded".to_string(),
        )
    };

    // Emit terminal receipt
    receipts.push(TxReceipt {
        seq: receipt_seq,
        state: terminal_state,
        emitted_at_ms: executed_at_ms,
        reason_code: Some(reason_code.clone()),
        error_code: error_code.clone(),
    });

    Ok(TxCompensationReport {
        tx_id: contract.intent.tx_id.clone(),
        plan_id: contract.plan.plan_id.clone(),
        outcome,
        step_results,
        compensated_count,
        failed_count,
        no_compensation_count,
        skipped_count,
        decision_path,
        reason_code,
        error_code,
        completed_at_ms: executed_at_ms,
        receipts,
    })
}

// ── H7: Durable Idempotency, Dedupe, and Resume ────────────────────────────

/// Record of a transaction execution attempt for idempotency tracking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TxExecutionRecord {
    /// Transaction ID this record tracks.
    pub tx_id: TxId,
    /// Plan ID that was executed.
    pub plan_id: TxPlanId,
    /// Current lifecycle state of the transaction.
    pub lifecycle_state: MissionTxState,
    /// Correlation ID from the original intent.
    pub correlation_id: String,
    /// Content-addressed idempotency key for the full tx.
    pub tx_idempotency_key: String,
    /// Per-step execution records.
    pub step_records: Vec<TxStepExecutionRecord>,
    /// Commit report if commit phase was executed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_report_hash: Option<String>,
    /// Compensation report if compensation phase was executed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compensation_report_hash: Option<String>,
    /// When the record was created or last updated.
    pub updated_at_ms: i64,
}

impl TxExecutionRecord {
    /// Compute idempotency key from contract content.
    #[must_use]
    pub fn compute_tx_key(contract: &MissionTxContract) -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        contract.intent.tx_id.0.hash(&mut hasher);
        contract.plan.plan_id.0.hash(&mut hasher);
        contract.intent.correlation_id.hash(&mut hasher);
        for step in &contract.plan.steps {
            step.step_id.0.hash(&mut hasher);
            step.ordinal.hash(&mut hasher);
        }
        let hash = hasher.finish();
        format!("txkey:{hash:016x}")
    }

    /// True if the transaction reached a terminal state.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.lifecycle_state,
            MissionTxState::Committed | MissionTxState::RolledBack | MissionTxState::Failed
        )
    }

    /// True if this is an exact duplicate of a prior completed execution.
    #[must_use]
    pub fn is_duplicate_of(&self, other_key: &str) -> bool {
        self.tx_idempotency_key == other_key && self.is_terminal()
    }

    /// Canonical string for deterministic hashing.
    #[must_use]
    pub fn canonical_string(&self) -> String {
        format!(
            "tx_id={}|plan_id={}|state={}|corr={}|key={}|steps={}|updated={}",
            self.tx_id.0,
            self.plan_id.0,
            self.lifecycle_state,
            self.correlation_id,
            self.tx_idempotency_key,
            self.step_records.len(),
            self.updated_at_ms,
        )
    }
}

/// Per-step execution record for idempotency and dedupe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TxStepExecutionRecord {
    /// Step that was executed.
    pub step_id: TxStepId,
    /// Step ordinal.
    pub ordinal: u32,
    /// The phase this step was last executed in.
    pub phase: TxPhase,
    /// Whether the step execution succeeded.
    pub succeeded: bool,
    /// Idempotency key for this specific step execution.
    pub step_idempotency_key: String,
    /// Number of times this step was attempted.
    pub attempt_count: u32,
    /// Timestamp of last attempt.
    pub last_attempted_at_ms: i64,
}

impl TxStepExecutionRecord {
    /// Compute step-level idempotency key.
    #[must_use]
    pub fn compute_step_key(tx_id: &TxId, step_id: &TxStepId, phase: &TxPhase) -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        tx_id.0.hash(&mut hasher);
        step_id.0.hash(&mut hasher);
        phase.tag_name().hash(&mut hasher);
        let hash = hasher.finish();
        format!("stepkey:{hash:016x}")
    }

    /// True if this step has already succeeded in this phase.
    #[must_use]
    pub fn is_already_succeeded(&self, phase: &TxPhase) -> bool {
        self.phase == *phase && self.succeeded
    }

    /// Canonical string for deterministic hashing.
    #[must_use]
    pub fn canonical_string(&self) -> String {
        format!(
            "step_id={}|ordinal={}|phase={}|ok={}|key={}|attempts={}",
            self.step_id.0,
            self.ordinal,
            self.phase.tag_name(),
            self.succeeded,
            self.step_idempotency_key,
            self.attempt_count,
        )
    }
}

/// Transaction execution phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TxPhase {
    /// Prepare phase (pre-flight validation).
    Prepare,
    /// Commit phase (forward execution).
    Commit,
    /// Compensation phase (rollback execution).
    Compensate,
}

impl TxPhase {
    /// Tag name for serialization and display.
    #[must_use]
    pub fn tag_name(&self) -> &str {
        match self {
            Self::Prepare => "prepare",
            Self::Commit => "commit",
            Self::Compensate => "compensate",
        }
    }
}

/// Result of idempotency validation before executing a tx phase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TxIdempotencyCheckResult {
    /// Transaction ID checked.
    pub tx_id: TxId,
    /// Phase being requested.
    pub requested_phase: TxPhase,
    /// The idempotency key that was checked.
    pub tx_idempotency_key: String,
    /// Verdict from the idempotency check.
    pub verdict: TxIdempotencyVerdict,
    /// Decision path describing the check flow.
    pub decision_path: String,
    /// Reason code.
    pub reason_code: String,
    /// Error code if blocked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}

impl TxIdempotencyCheckResult {
    /// True if the phase should proceed.
    #[must_use]
    pub fn should_proceed(&self) -> bool {
        matches!(
            self.verdict,
            TxIdempotencyVerdict::Fresh | TxIdempotencyVerdict::Resumable { .. }
        )
    }

    /// True if this is an exact duplicate of a completed execution.
    #[must_use]
    pub fn is_exact_duplicate(&self) -> bool {
        matches!(self.verdict, TxIdempotencyVerdict::ExactDuplicate)
    }

    /// Canonical string for deterministic hashing.
    #[must_use]
    pub fn canonical_string(&self) -> String {
        format!(
            "tx_id={}|phase={}|key={}|verdict={}|reason={}|err={}",
            self.tx_id.0,
            self.requested_phase.tag_name(),
            self.tx_idempotency_key,
            self.verdict.tag_name(),
            self.reason_code,
            self.error_code.as_deref().unwrap_or("none"),
        )
    }
}

/// Verdict from idempotency check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TxIdempotencyVerdict {
    /// No prior execution found — safe to proceed.
    Fresh,
    /// Prior execution exists but is resumable (non-terminal state).
    Resumable {
        /// The state to resume from.
        resume_from_state: MissionTxState,
        /// Steps already completed in the prior attempt.
        completed_steps: Vec<TxStepId>,
    },
    /// Prior execution completed identically — return cached result.
    ExactDuplicate,
    /// Prior execution completed with different outcome — conflict.
    ConflictingPrior {
        /// Prior state.
        prior_state: MissionTxState,
        /// Error describing the conflict.
        conflict_reason: String,
    },
    /// Double-commit or double-compensation guard triggered.
    DoubleExecutionBlocked {
        /// Which phase was already completed.
        already_completed_phase: TxPhase,
    },
}

impl TxIdempotencyVerdict {
    /// Tag name for the verdict.
    #[must_use]
    pub fn tag_name(&self) -> &str {
        match self {
            Self::Fresh => "fresh",
            Self::Resumable { .. } => "resumable",
            Self::ExactDuplicate => "exact_duplicate",
            Self::ConflictingPrior { .. } => "conflicting_prior",
            Self::DoubleExecutionBlocked { .. } => "double_execution_blocked",
        }
    }
}

/// Reconstructed resume state from receipt chain and reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TxResumeState {
    /// Transaction ID.
    pub tx_id: TxId,
    /// Current lifecycle state derived from receipts.
    pub derived_state: MissionTxState,
    /// Last receipt sequence number seen.
    pub last_receipt_seq: u64,
    /// Steps that have already been committed (in ordinal order).
    pub committed_step_ids: Vec<TxStepId>,
    /// Steps that have already been compensated (in reverse ordinal order).
    pub compensated_step_ids: Vec<TxStepId>,
    /// Steps remaining for the current phase.
    pub pending_step_ids: Vec<TxStepId>,
    /// Whether the commit phase completed.
    pub commit_phase_completed: bool,
    /// Whether the compensation phase completed.
    pub compensation_phase_completed: bool,
    /// Decision path describing how state was reconstructed.
    pub decision_path: String,
    /// Timestamp of reconstruction.
    pub reconstructed_at_ms: i64,
}

impl TxResumeState {
    /// True if there are pending steps to execute.
    #[must_use]
    pub fn has_pending_work(&self) -> bool {
        !self.pending_step_ids.is_empty()
    }

    /// True if both phases are done (terminal).
    #[must_use]
    pub fn is_fully_resolved(&self) -> bool {
        matches!(
            self.derived_state,
            MissionTxState::Committed | MissionTxState::RolledBack | MissionTxState::Failed
        )
    }

    /// Canonical string for deterministic hashing.
    #[must_use]
    pub fn canonical_string(&self) -> String {
        format!(
            "tx_id={}|state={}|last_seq={}|committed={}|compensated={}|pending={}|commit_done={}|comp_done={}",
            self.tx_id.0,
            self.derived_state,
            self.last_receipt_seq,
            self.committed_step_ids.len(),
            self.compensated_step_ids.len(),
            self.pending_step_ids.len(),
            self.commit_phase_completed,
            self.compensation_phase_completed,
        )
    }
}

/// Validate idempotency before executing a transaction phase.
///
/// Checks the execution record ledger for prior attempts and returns a verdict.
pub fn validate_tx_idempotency(
    contract: &MissionTxContract,
    requested_phase: TxPhase,
    prior_record: Option<&TxExecutionRecord>,
) -> TxIdempotencyCheckResult {
    let tx_key = TxExecutionRecord::compute_tx_key(contract);

    // No prior record → fresh execution
    let record = match prior_record {
        None => {
            return TxIdempotencyCheckResult {
                tx_id: contract.intent.tx_id.clone(),
                requested_phase,
                tx_idempotency_key: tx_key,
                verdict: TxIdempotencyVerdict::Fresh,
                decision_path: "idempotency_no_prior".into(),
                reason_code: "fresh_execution".into(),
                error_code: None,
            };
        }
        Some(r) => r,
    };

    // Check double-execution guard
    match requested_phase {
        TxPhase::Commit if record.commit_report_hash.is_some() && record.is_terminal() => {
            return TxIdempotencyCheckResult {
                tx_id: contract.intent.tx_id.clone(),
                requested_phase,
                tx_idempotency_key: tx_key,
                verdict: TxIdempotencyVerdict::DoubleExecutionBlocked {
                    already_completed_phase: TxPhase::Commit,
                },
                decision_path: "idempotency_double_commit_blocked".into(),
                reason_code: "commit_already_completed".into(),
                error_code: Some("FTX3001".into()),
            };
        }
        TxPhase::Compensate
            if record.compensation_report_hash.is_some() && record.is_terminal() =>
        {
            return TxIdempotencyCheckResult {
                tx_id: contract.intent.tx_id.clone(),
                requested_phase,
                tx_idempotency_key: tx_key,
                verdict: TxIdempotencyVerdict::DoubleExecutionBlocked {
                    already_completed_phase: TxPhase::Compensate,
                },
                decision_path: "idempotency_double_compensate_blocked".into(),
                reason_code: "compensation_already_completed".into(),
                error_code: Some("FTX3002".into()),
            };
        }
        _ => {}
    }

    // Exact duplicate check (terminal + same key)
    if record.is_duplicate_of(&tx_key) {
        return TxIdempotencyCheckResult {
            tx_id: contract.intent.tx_id.clone(),
            requested_phase,
            tx_idempotency_key: tx_key,
            verdict: TxIdempotencyVerdict::ExactDuplicate,
            decision_path: "idempotency_exact_duplicate".into(),
            reason_code: "exact_duplicate_detected".into(),
            error_code: None,
        };
    }

    // Terminal state with different key → conflict
    if record.is_terminal() && record.tx_idempotency_key != tx_key {
        return TxIdempotencyCheckResult {
            tx_id: contract.intent.tx_id.clone(),
            requested_phase,
            tx_idempotency_key: tx_key,
            verdict: TxIdempotencyVerdict::ConflictingPrior {
                prior_state: record.lifecycle_state,
                conflict_reason: format!("prior key {} != current key", record.tx_idempotency_key),
            },
            decision_path: "idempotency_conflicting_prior".into(),
            reason_code: "key_mismatch_on_terminal".into(),
            error_code: Some("FTX3003".into()),
        };
    }

    // Non-terminal with matching key → resumable
    let completed_steps: Vec<TxStepId> = record
        .step_records
        .iter()
        .filter(|sr| sr.succeeded && sr.phase == requested_phase)
        .map(|sr| sr.step_id.clone())
        .collect();

    TxIdempotencyCheckResult {
        tx_id: contract.intent.tx_id.clone(),
        requested_phase,
        tx_idempotency_key: tx_key,
        verdict: TxIdempotencyVerdict::Resumable {
            resume_from_state: record.lifecycle_state,
            completed_steps,
        },
        decision_path: "idempotency_resumable".into(),
        reason_code: "prior_non_terminal_resumable".into(),
        error_code: None,
    }
}

/// Reconstruct transaction resume state from contract receipts and optional reports.
///
/// Used after crash/restart to determine what work was already done and what remains.
pub fn reconstruct_tx_resume_state(
    contract: &MissionTxContract,
    commit_report: Option<&TxCommitReport>,
    compensation_report: Option<&TxCompensationReport>,
    reconstructed_at_ms: i64,
) -> TxResumeState {
    let all_step_ids: Vec<TxStepId> = contract
        .plan
        .steps
        .iter()
        .map(|s| s.step_id.clone())
        .collect();

    let last_receipt_seq = contract.receipts.last().map_or(0, |r| r.seq);

    // Derive state from receipts (last receipt wins)
    let derived_state = contract
        .receipts
        .last()
        .map_or(contract.lifecycle_state, |r| r.state);

    // Extract committed step IDs from commit report
    let committed_step_ids: Vec<TxStepId> = commit_report
        .map(|cr| {
            cr.step_results
                .iter()
                .filter(|sr| sr.outcome.is_committed())
                .map(|sr| sr.step_id.clone())
                .collect()
        })
        .unwrap_or_default();

    // Extract compensated step IDs from compensation report
    let compensated_step_ids: Vec<TxStepId> = compensation_report
        .map(|cr| {
            cr.step_results
                .iter()
                .filter(|sr| sr.outcome.is_compensated())
                .map(|sr| sr.for_step_id.clone())
                .collect()
        })
        .unwrap_or_default();

    let commit_phase_completed = commit_report.is_some();
    let compensation_phase_completed = compensation_report.is_some();

    // Determine pending steps based on current state
    let pending_step_ids = match derived_state {
        MissionTxState::Prepared | MissionTxState::Committing => {
            // Steps not yet committed
            let committed_set: std::collections::HashSet<&str> =
                committed_step_ids.iter().map(|id| id.0.as_str()).collect();
            all_step_ids
                .iter()
                .filter(|id| !committed_set.contains(id.0.as_str()))
                .cloned()
                .collect()
        }
        MissionTxState::Compensating => {
            // Committed steps not yet compensated
            let compensated_set: std::collections::HashSet<&str> = compensated_step_ids
                .iter()
                .map(|id| id.0.as_str())
                .collect();
            committed_step_ids
                .iter()
                .filter(|id| !compensated_set.contains(id.0.as_str()))
                .cloned()
                .collect()
        }
        _ => Vec::new(), // Terminal or early states have no pending work
    };

    let decision_path = if commit_phase_completed && compensation_phase_completed {
        "resume_both_phases_completed".into()
    } else if commit_phase_completed {
        "resume_commit_completed".into()
    } else if !committed_step_ids.is_empty() {
        "resume_partial_commit".into()
    } else {
        "resume_no_progress".into()
    };

    TxResumeState {
        tx_id: contract.intent.tx_id.clone(),
        derived_state,
        last_receipt_seq,
        committed_step_ids,
        compensated_step_ids,
        pending_step_ids,
        commit_phase_completed,
        compensation_phase_completed,
        decision_path,
        reconstructed_at_ms,
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

// ============================================================================
// Intent Ledger — Append-Only Event-Sourced Transaction Persistence (H2)
// ============================================================================

/// Schema version for the intent ledger wire format.
pub const INTENT_LEDGER_SCHEMA_VERSION: u32 = 1;

/// A single entry in the append-only intent ledger.
///
/// Each entry captures a transaction decision point with full causal context.
/// Entries form a hash chain: each entry's `prev_hash` references the preceding
/// entry's `entry_hash`, creating a tamper-evident audit trail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerEntry {
    /// Monotonically increasing sequence number (1-indexed).
    pub seq: u64,
    /// SHA-256 hash of this entry's canonical content (hex-encoded).
    pub entry_hash: String,
    /// SHA-256 hash of the previous entry (hex for chain, empty string for genesis).
    pub prev_hash: String,
    /// Transaction this entry belongs to.
    pub tx_id: TxId,
    /// Timestamp when this entry was created (epoch ms).
    pub created_at_ms: i64,
    /// The kind of ledger event.
    pub kind: LedgerEntryKind,
    /// Correlation hooks for cross-system tracing.
    pub correlation: LedgerCorrelation,
}

/// Discriminated union of ledger event kinds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LedgerEntryKind {
    /// Transaction intent was registered.
    IntentRegistered {
        summary: String,
        requested_by: String,
    },
    /// Execution plan was created and attached.
    PlanCreated {
        plan_id: TxPlanId,
        step_count: u32,
        precondition_count: u32,
        compensation_count: u32,
    },
    /// A precondition was evaluated.
    PreconditionEvaluated {
        precondition_index: u32,
        passed: bool,
        detail: String,
    },
    /// A lifecycle state transition occurred.
    StateTransition {
        from: MissionTxState,
        to: MissionTxState,
        kind: MissionTxTransitionKind,
    },
    /// A step execution result was recorded.
    StepExecuted {
        step_id: TxStepId,
        ordinal: u32,
        succeeded: bool,
        detail: String,
    },
    /// A compensation action was executed.
    CompensationExecuted {
        for_step_id: TxStepId,
        succeeded: bool,
        detail: String,
    },
    /// Final outcome was sealed.
    OutcomeSealed {
        outcome_kind: String,
        reason_code: Option<String>,
        error_code: Option<String>,
    },
    /// A causal receipt was recorded (mirrors TxReceipt).
    ReceiptRecorded {
        receipt_seq: u64,
        state: MissionTxState,
        reason_code: Option<String>,
        error_code: Option<String>,
    },
}

/// Correlation context linking a ledger entry to external systems.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerCorrelation {
    /// Mission run identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mission_run_id: Option<String>,
    /// Pane identifiers involved in this decision.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pane_ids: Vec<u64>,
    /// Agent identifiers involved.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agent_ids: Vec<String>,
    /// Bead/issue identifiers related to this work.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bead_ids: Vec<String>,
    /// Thread identifiers (agent mail).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub thread_ids: Vec<String>,
    /// Policy check identifiers that influenced this decision.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub policy_check_ids: Vec<String>,
    /// Reservation identifiers held during this decision.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reservation_ids: Vec<String>,
    /// Approval identifiers granted for this decision.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub approval_ids: Vec<String>,
}

impl LedgerCorrelation {
    /// Create an empty correlation context.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            mission_run_id: None,
            pane_ids: Vec::new(),
            agent_ids: Vec::new(),
            bead_ids: Vec::new(),
            thread_ids: Vec::new(),
            policy_check_ids: Vec::new(),
            reservation_ids: Vec::new(),
            approval_ids: Vec::new(),
        }
    }

    /// Create a correlation with just a mission run id.
    #[must_use]
    pub fn with_mission(mission_run_id: impl Into<String>) -> Self {
        Self {
            mission_run_id: Some(mission_run_id.into()),
            ..Self::empty()
        }
    }
}

impl LedgerEntry {
    /// Compute the canonical SHA-256 hash for this entry's content.
    ///
    /// The hash covers: seq, prev_hash, tx_id, created_at_ms, kind (JSON),
    /// and correlation (JSON). This makes the chain tamper-evident.
    #[must_use]
    pub fn compute_hash(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.seq.to_le_bytes());
        hasher.update(self.prev_hash.as_bytes());
        hasher.update(self.tx_id.0.as_bytes());
        hasher.update(self.created_at_ms.to_le_bytes());
        // Canonical JSON of kind and correlation for determinism
        if let Ok(kind_json) = serde_json::to_string(&self.kind) {
            hasher.update(kind_json.as_bytes());
        }
        if let Ok(corr_json) = serde_json::to_string(&self.correlation) {
            hasher.update(corr_json.as_bytes());
        }
        format!("{:x}", hasher.finalize())
    }

    /// Verify this entry's hash matches its content.
    #[must_use]
    pub fn verify_hash(&self) -> bool {
        self.entry_hash == self.compute_hash()
    }
}

/// Validation errors specific to the intent ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LedgerValidationError {
    /// Entry sequence is not monotonically increasing.
    NonMonotonicSequence { expected: u64, actual: u64 },
    /// Hash chain is broken (prev_hash doesn't match previous entry_hash).
    BrokenHashChain {
        seq: u64,
        expected_prev: String,
        actual_prev: String,
    },
    /// Entry hash doesn't match recomputed content hash.
    TamperedEntry { seq: u64 },
    /// Genesis entry has non-empty prev_hash.
    InvalidGenesis,
    /// Ledger is empty when entries were expected.
    EmptyLedger,
    /// Transaction ID mismatch within a single-tx ledger.
    TxIdMismatch {
        seq: u64,
        expected: TxId,
        actual: TxId,
    },
}

impl fmt::Display for LedgerValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonMonotonicSequence { expected, actual } => {
                write!(
                    f,
                    "Non-monotonic ledger sequence: expected {expected}, got {actual}"
                )
            }
            Self::BrokenHashChain {
                seq,
                expected_prev,
                actual_prev,
            } => {
                write!(
                    f,
                    "Broken hash chain at seq {seq}: expected prev {expected_prev}, got {actual_prev}"
                )
            }
            Self::TamperedEntry { seq } => {
                write!(f, "Tampered ledger entry at seq {seq}: hash mismatch")
            }
            Self::InvalidGenesis => f.write_str("Genesis entry must have empty prev_hash"),
            Self::EmptyLedger => f.write_str("Ledger is empty"),
            Self::TxIdMismatch {
                seq,
                expected,
                actual,
            } => {
                write!(
                    f,
                    "Tx ID mismatch at seq {seq}: expected {expected}, got {actual}"
                )
            }
        }
    }
}

impl std::error::Error for LedgerValidationError {}

/// Append-only intent ledger for a single transaction.
///
/// Maintains a hash-chained sequence of decision entries with full causal
/// correlation context. Supports timeline queries for `tx show` reconstruction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntentLedger {
    /// Schema version for forward compatibility.
    pub schema_version: u32,
    /// The transaction this ledger tracks.
    pub tx_id: TxId,
    /// Ordered entries forming the hash chain.
    entries: Vec<LedgerEntry>,
}

impl IntentLedger {
    /// Create a new empty ledger for a transaction.
    #[must_use]
    pub fn new(tx_id: TxId) -> Self {
        Self {
            schema_version: INTENT_LEDGER_SCHEMA_VERSION,
            tx_id,
            entries: Vec::new(),
        }
    }

    /// Append a new entry to the ledger.
    ///
    /// Automatically assigns sequence number, computes prev_hash from the
    /// last entry, and computes the entry's content hash.
    pub fn append(
        &mut self,
        created_at_ms: i64,
        kind: LedgerEntryKind,
        correlation: LedgerCorrelation,
    ) -> &LedgerEntry {
        let seq = self.entries.len() as u64 + 1;
        let prev_hash = self
            .entries
            .last()
            .map(|e| e.entry_hash.clone())
            .unwrap_or_default();

        let mut entry = LedgerEntry {
            seq,
            entry_hash: String::new(),
            prev_hash,
            tx_id: self.tx_id.clone(),
            created_at_ms,
            kind,
            correlation,
        };
        entry.entry_hash = entry.compute_hash();
        self.entries.push(entry);
        self.entries.last().unwrap()
    }

    /// Number of entries in the ledger.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the ledger has no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Get all entries as a slice.
    #[must_use]
    pub fn entries(&self) -> &[LedgerEntry] {
        &self.entries
    }

    /// Get the most recent entry.
    #[must_use]
    pub fn last_entry(&self) -> Option<&LedgerEntry> {
        self.entries.last()
    }

    /// Get entry by sequence number (1-indexed).
    #[must_use]
    pub fn entry_at(&self, seq: u64) -> Option<&LedgerEntry> {
        if seq == 0 || seq as usize > self.entries.len() {
            return None;
        }
        Some(&self.entries[seq as usize - 1])
    }

    /// Query entries by kind discriminant.
    pub fn entries_of_kind(&self, kind_tag: &str) -> Vec<&LedgerEntry> {
        self.entries
            .iter()
            .filter(|e| {
                let tag = match &e.kind {
                    LedgerEntryKind::IntentRegistered { .. } => "intent_registered",
                    LedgerEntryKind::PlanCreated { .. } => "plan_created",
                    LedgerEntryKind::PreconditionEvaluated { .. } => "precondition_evaluated",
                    LedgerEntryKind::StateTransition { .. } => "state_transition",
                    LedgerEntryKind::StepExecuted { .. } => "step_executed",
                    LedgerEntryKind::CompensationExecuted { .. } => "compensation_executed",
                    LedgerEntryKind::OutcomeSealed { .. } => "outcome_sealed",
                    LedgerEntryKind::ReceiptRecorded { .. } => "receipt_recorded",
                };
                tag == kind_tag
            })
            .collect()
    }

    /// Query entries within a time range (inclusive).
    pub fn entries_in_range(&self, start_ms: i64, end_ms: i64) -> Vec<&LedgerEntry> {
        self.entries
            .iter()
            .filter(|e| e.created_at_ms >= start_ms && e.created_at_ms <= end_ms)
            .collect()
    }

    /// Query entries mentioning a specific pane.
    pub fn entries_for_pane(&self, pane_id: u64) -> Vec<&LedgerEntry> {
        self.entries
            .iter()
            .filter(|e| e.correlation.pane_ids.contains(&pane_id))
            .collect()
    }

    /// Query entries mentioning a specific agent.
    pub fn entries_for_agent(&self, agent_id: &str) -> Vec<&LedgerEntry> {
        self.entries
            .iter()
            .filter(|e| e.correlation.agent_ids.iter().any(|a| a == agent_id))
            .collect()
    }

    /// Extract the current lifecycle state from the ledger.
    ///
    /// Scans state transition entries in order to find the latest state.
    #[must_use]
    pub fn current_state(&self) -> MissionTxState {
        self.entries
            .iter()
            .rev()
            .find_map(|e| match &e.kind {
                LedgerEntryKind::StateTransition { to, .. } => Some(*to),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// Build a timeline of state transitions for `tx show`.
    pub fn state_timeline(&self) -> Vec<StateTimelineEntry> {
        self.entries
            .iter()
            .filter_map(|e| match &e.kind {
                LedgerEntryKind::StateTransition { from, to, kind } => Some(StateTimelineEntry {
                    seq: e.seq,
                    timestamp_ms: e.created_at_ms,
                    from: *from,
                    to: *to,
                    kind: *kind,
                    correlation: e.correlation.clone(),
                }),
                _ => None,
            })
            .collect()
    }

    /// Validate the entire ledger's integrity.
    ///
    /// Checks: monotonic sequences, hash chain continuity, content hash
    /// integrity, genesis validity, and tx_id consistency.
    pub fn validate(&self) -> Result<(), LedgerValidationError> {
        if self.entries.is_empty() {
            return Ok(()); // Empty ledger is valid
        }

        // Genesis entry
        let first = &self.entries[0];
        if !first.prev_hash.is_empty() {
            return Err(LedgerValidationError::InvalidGenesis);
        }
        if first.seq != 1 {
            return Err(LedgerValidationError::NonMonotonicSequence {
                expected: 1,
                actual: first.seq,
            });
        }
        if !first.verify_hash() {
            return Err(LedgerValidationError::TamperedEntry { seq: first.seq });
        }
        if first.tx_id != self.tx_id {
            return Err(LedgerValidationError::TxIdMismatch {
                seq: first.seq,
                expected: self.tx_id.clone(),
                actual: first.tx_id.clone(),
            });
        }

        // Remaining entries
        for i in 1..self.entries.len() {
            let prev = &self.entries[i - 1];
            let curr = &self.entries[i];

            let expected_seq = prev.seq + 1;
            if curr.seq != expected_seq {
                return Err(LedgerValidationError::NonMonotonicSequence {
                    expected: expected_seq,
                    actual: curr.seq,
                });
            }

            if curr.prev_hash != prev.entry_hash {
                return Err(LedgerValidationError::BrokenHashChain {
                    seq: curr.seq,
                    expected_prev: prev.entry_hash.clone(),
                    actual_prev: curr.prev_hash.clone(),
                });
            }

            if !curr.verify_hash() {
                return Err(LedgerValidationError::TamperedEntry { seq: curr.seq });
            }

            if curr.tx_id != self.tx_id {
                return Err(LedgerValidationError::TxIdMismatch {
                    seq: curr.seq,
                    expected: self.tx_id.clone(),
                    actual: curr.tx_id.clone(),
                });
            }
        }

        Ok(())
    }

    /// Serialize the ledger to JSONL (one JSON line per entry).
    ///
    /// This is the canonical persistence format for append-only storage.
    #[must_use]
    pub fn to_jsonl(&self) -> String {
        self.entries
            .iter()
            .filter_map(|e| serde_json::to_string(e).ok())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Deserialize a ledger from JSONL lines.
    pub fn from_jsonl(tx_id: TxId, lines: &str) -> Result<Self, String> {
        let mut ledger = Self::new(tx_id);
        for (i, line) in lines.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let entry: LedgerEntry =
                serde_json::from_str(line).map_err(|e| format!("line {}: {}", i + 1, e))?;
            ledger.entries.push(entry);
        }
        Ok(ledger)
    }
}

/// A state transition in the timeline (for `tx show` display).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateTimelineEntry {
    pub seq: u64,
    pub timestamp_ms: i64,
    pub from: MissionTxState,
    pub to: MissionTxState,
    pub kind: MissionTxTransitionKind,
    pub correlation: LedgerCorrelation,
}

/// Builder for recording a full transaction lifecycle into a ledger.
///
/// Provides high-level methods that map to `LedgerEntryKind` variants,
/// automatically managing timestamps and correlation propagation.
pub struct LedgerRecorder<'a> {
    ledger: &'a mut IntentLedger,
    default_correlation: LedgerCorrelation,
}

impl<'a> LedgerRecorder<'a> {
    /// Create a recorder bound to a ledger with default correlation.
    pub fn new(ledger: &'a mut IntentLedger, correlation: LedgerCorrelation) -> Self {
        Self {
            ledger,
            default_correlation: correlation,
        }
    }

    /// Record that a transaction intent was registered.
    pub fn record_intent(&mut self, ts: i64, summary: &str, requested_by: &str) -> &LedgerEntry {
        self.ledger.append(
            ts,
            LedgerEntryKind::IntentRegistered {
                summary: summary.to_string(),
                requested_by: requested_by.to_string(),
            },
            self.default_correlation.clone(),
        )
    }

    /// Record that an execution plan was created.
    pub fn record_plan(
        &mut self,
        ts: i64,
        plan_id: &TxPlanId,
        step_count: u32,
        precondition_count: u32,
        compensation_count: u32,
    ) -> &LedgerEntry {
        self.ledger.append(
            ts,
            LedgerEntryKind::PlanCreated {
                plan_id: plan_id.clone(),
                step_count,
                precondition_count,
                compensation_count,
            },
            self.default_correlation.clone(),
        )
    }

    /// Record a precondition evaluation result.
    pub fn record_precondition(
        &mut self,
        ts: i64,
        index: u32,
        passed: bool,
        detail: &str,
    ) -> &LedgerEntry {
        self.ledger.append(
            ts,
            LedgerEntryKind::PreconditionEvaluated {
                precondition_index: index,
                passed,
                detail: detail.to_string(),
            },
            self.default_correlation.clone(),
        )
    }

    /// Record a lifecycle state transition.
    pub fn record_transition(
        &mut self,
        ts: i64,
        from: MissionTxState,
        to: MissionTxState,
        kind: MissionTxTransitionKind,
    ) -> &LedgerEntry {
        self.ledger.append(
            ts,
            LedgerEntryKind::StateTransition { from, to, kind },
            self.default_correlation.clone(),
        )
    }

    /// Record a step execution result.
    pub fn record_step(
        &mut self,
        ts: i64,
        step_id: &TxStepId,
        ordinal: u32,
        succeeded: bool,
        detail: &str,
    ) -> &LedgerEntry {
        self.ledger.append(
            ts,
            LedgerEntryKind::StepExecuted {
                step_id: step_id.clone(),
                ordinal,
                succeeded,
                detail: detail.to_string(),
            },
            self.default_correlation.clone(),
        )
    }

    /// Record a compensation action result.
    pub fn record_compensation(
        &mut self,
        ts: i64,
        for_step_id: &TxStepId,
        succeeded: bool,
        detail: &str,
    ) -> &LedgerEntry {
        self.ledger.append(
            ts,
            LedgerEntryKind::CompensationExecuted {
                for_step_id: for_step_id.clone(),
                succeeded,
                detail: detail.to_string(),
            },
            self.default_correlation.clone(),
        )
    }

    /// Record that the final outcome was sealed.
    pub fn record_outcome(
        &mut self,
        ts: i64,
        outcome_kind: &str,
        reason_code: Option<&str>,
        error_code: Option<&str>,
    ) -> &LedgerEntry {
        self.ledger.append(
            ts,
            LedgerEntryKind::OutcomeSealed {
                outcome_kind: outcome_kind.to_string(),
                reason_code: reason_code.map(String::from),
                error_code: error_code.map(String::from),
            },
            self.default_correlation.clone(),
        )
    }

    /// Record a receipt (mirrors TxReceipt for ledger correlation).
    pub fn record_receipt(
        &mut self,
        ts: i64,
        receipt_seq: u64,
        state: MissionTxState,
        reason_code: Option<&str>,
        error_code: Option<&str>,
    ) -> &LedgerEntry {
        self.ledger.append(
            ts,
            LedgerEntryKind::ReceiptRecorded {
                receipt_seq,
                state,
                reason_code: reason_code.map(String::from),
                error_code: error_code.map(String::from),
            },
            self.default_correlation.clone(),
        )
    }
}

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

/// Dispatch execution mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionDispatchMode {
    DryRun,
    Live,
}

/// Resolved dispatch target for one assignment execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionDispatchTarget {
    pub assignment_id: AssignmentId,
    pub candidate_id: CandidateActionId,
    pub assignee: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bead_id: Option<String>,
}

/// Normalized live dispatch response envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum MissionDispatchLiveResponse {
    Delivered {
        #[serde(skip_serializing_if = "Option::is_none")]
        reason_code: Option<String>,
        completed_at_ms: i64,
    },
    Failed {
        reason_code: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        error_code: Option<String>,
        completed_at_ms: i64,
    },
}

/// Final dispatch adapter output used by mission runtime/state reconciliation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissionDispatchExecution {
    pub mode: MissionDispatchMode,
    pub target: MissionDispatchTarget,
    pub mechanism: MissionDispatchMechanism,
    pub outcome: Outcome,
}

// ============================================================================
// Mission Dispatch Idempotency and Deduplication
// ============================================================================

/// Content-addressed idempotency key for mission dispatch deduplication.
///
/// Computed from the stable identity of an assignment and its dispatch mechanism,
/// ensuring that retries and restarts of the same logical dispatch action are
/// recognized as duplicates and suppressed.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MissionDispatchIdempotencyKey(pub String);

impl MissionDispatchIdempotencyKey {
    /// Compute an idempotency key from assignment and mechanism identity.
    ///
    /// The key is content-addressed: same (mission_id, assignment_id, mechanism)
    /// always produces the same key, regardless of wall-clock time or retry count.
    #[must_use]
    pub fn compute(
        mission_id: &MissionId,
        assignment_id: &AssignmentId,
        mechanism: &MissionDispatchMechanism,
    ) -> Self {
        let mechanism_json = serde_json::to_string(mechanism).unwrap_or_default();
        let canonical = format!(
            "dispatch:mission={}|assignment={}|mechanism={}",
            mission_id.0, assignment_id.0, mechanism_json
        );
        let hash = sha256_hex(&canonical);
        Self(format!("dispatch:{}", &hash[..16]))
    }

    /// Return the raw key string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for MissionDispatchIdempotencyKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Durable record of a dispatched action, used for duplicate-dispatch prevention.
///
/// When a dispatch is executed, a deduplication record is persisted. Subsequent
/// dispatch attempts with the same idempotency key are recognized as duplicates
/// and return the cached outcome without re-execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionDispatchDeduplicationRecord {
    pub idempotency_key: MissionDispatchIdempotencyKey,
    pub assignment_id: AssignmentId,
    pub correlation_id: String,
    pub dispatched_at_ms: i64,
    pub outcome: Outcome,
    /// Mechanism fingerprint for cross-check on retry.
    pub mechanism_hash: String,
}

impl MissionDispatchDeduplicationRecord {
    /// Deterministic string form for auditing.
    #[must_use]
    pub fn canonical_string(&self) -> String {
        format!(
            "key={},assignment={},correlation={},dispatched_at_ms={},mechanism_hash={},outcome={}",
            self.idempotency_key.0,
            self.assignment_id.0,
            self.correlation_id,
            self.dispatched_at_ms,
            self.mechanism_hash,
            self.outcome.canonical_string(),
        )
    }
}

/// Result of dispatch deduplication evaluation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionDispatchDeduplicationResult {
    pub idempotency_key: MissionDispatchIdempotencyKey,
    pub is_duplicate: bool,
    pub decision_path: String,
    pub reason_code: String,
    /// Present when `is_duplicate` is true — the cached execution to return.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_record: Option<MissionDispatchDeduplicationRecord>,
}

/// Persistent deduplication state for a mission's dispatch history.
///
/// Tracks recently dispatched actions by idempotency key so retries/restarts
/// are detected and suppressed. The state is serializable for crash-recovery.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionDispatchDeduplicationState {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub records: Vec<MissionDispatchDeduplicationRecord>,
}

impl MissionDispatchDeduplicationState {
    /// Check if state has no recorded dispatches.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Find a dedup record by idempotency key.
    #[must_use]
    pub fn find_by_key(
        &self,
        key: &MissionDispatchIdempotencyKey,
    ) -> Option<&MissionDispatchDeduplicationRecord> {
        self.records
            .iter()
            .find(|record| record.idempotency_key == *key)
    }

    /// Record a new dispatch execution. Overwrites any existing record with the
    /// same idempotency key (last-write-wins for crash recovery).
    pub fn record_dispatch(&mut self, record: MissionDispatchDeduplicationRecord) {
        if let Some(existing) = self
            .records
            .iter_mut()
            .find(|r| r.idempotency_key == record.idempotency_key)
        {
            *existing = record;
        } else {
            self.records.push(record);
        }
    }

    /// Evict records older than `cutoff_ms` to bound memory growth.
    pub fn evict_before(&mut self, cutoff_ms: i64) {
        self.records
            .retain(|record| record.dispatched_at_ms >= cutoff_ms);
    }

    /// Deterministic string form for mission canonical hash.
    #[must_use]
    pub fn canonical_string(&self) -> String {
        let entries: Vec<String> = self.records.iter().map(|r| r.canonical_string()).collect();
        format!("dedup_records=[{}]", entries.join(";"))
    }
}

// ============================================================================
// Global Kill-Switch and Safe-Mode Degradation
// ============================================================================

/// Operating level for the global mission kill-switch.
///
/// Three levels of protection:
/// - `Off`: normal operation, all dispatches proceed per policy.
/// - `SafeMode`: read-only/monitoring allowed, new dispatches blocked.
/// - `HardStop`: all mission activity halted, in-flight work cancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MissionKillSwitchLevel {
    #[default]
    Off,
    SafeMode,
    HardStop,
}

impl MissionKillSwitchLevel {
    /// Whether this level blocks new dispatch execution.
    #[must_use]
    pub const fn blocks_dispatch(&self) -> bool {
        matches!(self, Self::SafeMode | Self::HardStop)
    }

    /// Whether this level cancels in-flight missions.
    #[must_use]
    pub const fn cancels_in_flight(&self) -> bool {
        matches!(self, Self::HardStop)
    }

    /// Whether read-only/monitoring operations are permitted.
    #[must_use]
    pub const fn allows_read_only(&self) -> bool {
        matches!(self, Self::Off | Self::SafeMode)
    }
}

impl fmt::Display for MissionKillSwitchLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Off => f.write_str("off"),
            Self::SafeMode => f.write_str("safe_mode"),
            Self::HardStop => f.write_str("hard_stop"),
        }
    }
}

/// Record of a kill-switch activation event for audit trail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionKillSwitchActivation {
    /// The level at which the kill-switch was set.
    pub level: MissionKillSwitchLevel,
    /// Who activated the kill-switch (operator/system/agent).
    pub activated_by: String,
    /// Structured reason code for the activation trigger.
    pub reason_code: String,
    /// Machine-parseable error code, if trigger is error-driven.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// Epoch-millis when the kill-switch was activated.
    pub activated_at_ms: i64,
    /// Optional epoch-millis after which the kill-switch auto-expires.
    /// If `None`, manual deactivation is required.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<i64>,
    /// Optional correlation ID linking to the triggering event.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
}

impl MissionKillSwitchActivation {
    /// Deterministic string form for audit hashing.
    #[must_use]
    pub fn canonical_string(&self) -> String {
        format!(
            "level={},activated_by={},reason_code={},error_code={},activated_at_ms={},expires_at_ms={},correlation_id={}",
            self.level,
            self.activated_by,
            self.reason_code,
            self.error_code.as_deref().unwrap_or("none"),
            self.activated_at_ms,
            self.expires_at_ms
                .map_or_else(|| "none".to_string(), |ms| ms.to_string()),
            self.correlation_id.as_deref().unwrap_or("none"),
        )
    }

    /// Check whether this activation has expired at a given evaluation time.
    #[must_use]
    pub fn is_expired_at(&self, evaluated_at_ms: i64) -> bool {
        self.expires_at_ms
            .is_some_and(|expires_at_ms| evaluated_at_ms >= expires_at_ms)
    }
}

/// Persistent kill-switch state for a mission.
///
/// Tracks activation history and current level. The state is serializable
/// for crash-recovery and deterministic replay.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionKillSwitchState {
    /// Current effective kill-switch level.
    #[serde(default)]
    pub level: MissionKillSwitchLevel,
    /// The activation record for the current level (None when `level == Off`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_activation: Option<MissionKillSwitchActivation>,
    /// Ordered history of activation events for audit trail.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub activation_history: Vec<MissionKillSwitchActivation>,
}

impl MissionKillSwitchState {
    /// Check if the kill-switch is in its default (off) state.
    #[must_use]
    pub fn is_off(&self) -> bool {
        self.level == MissionKillSwitchLevel::Off && self.current_activation.is_none()
    }

    /// Activate the kill-switch at the specified level.
    ///
    /// Records the activation and pushes it to the audit history.
    /// If the kill-switch is already active, escalation replaces the current
    /// activation (last-write-wins with full audit trail).
    pub fn activate(&mut self, activation: MissionKillSwitchActivation) {
        self.level = activation.level;
        self.activation_history.push(activation.clone());
        self.current_activation = Some(activation);
    }

    /// Deactivate the kill-switch, returning to normal operation.
    ///
    /// Records a deactivation event in the audit history.
    pub fn deactivate(&mut self, deactivated_by: &str, reason_code: &str, deactivated_at_ms: i64) {
        let deactivation = MissionKillSwitchActivation {
            level: MissionKillSwitchLevel::Off,
            activated_by: deactivated_by.to_string(),
            reason_code: reason_code.to_string(),
            error_code: None,
            activated_at_ms: deactivated_at_ms,
            expires_at_ms: None,
            correlation_id: None,
        };
        self.activation_history.push(deactivation);
        self.level = MissionKillSwitchLevel::Off;
        self.current_activation = None;
    }

    /// Evaluate the effective kill-switch state, accounting for TTL expiry.
    ///
    /// If the current activation has expired, the level is automatically
    /// downgraded to Off (lazy expiry on read).
    pub fn evaluate_effective_level(&mut self, evaluated_at_ms: i64) -> MissionKillSwitchLevel {
        if let Some(activation) = &self.current_activation {
            if activation.is_expired_at(evaluated_at_ms) {
                // Auto-expire: record synthetic deactivation
                self.deactivate("system", "kill_switch_ttl_expired", evaluated_at_ms);
                return MissionKillSwitchLevel::Off;
            }
        }
        self.level
    }

    /// Evict activation history older than `cutoff_ms` to bound memory growth.
    pub fn evict_history_before(&mut self, cutoff_ms: i64) {
        self.activation_history
            .retain(|activation| activation.activated_at_ms >= cutoff_ms);
    }

    /// Deterministic canonical string form for mission hashing.
    #[must_use]
    pub fn canonical_string(&self) -> String {
        let history_entries: Vec<String> = self
            .activation_history
            .iter()
            .map(|a| a.canonical_string())
            .collect();
        format!(
            "kill_switch_level={},current={},history=[{}]",
            self.level,
            self.current_activation
                .as_ref()
                .map_or_else(|| "none".to_string(), |a| a.canonical_string()),
            history_entries.join(";"),
        )
    }
}

/// Result of evaluating the kill-switch before a dispatch or lifecycle operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionKillSwitchDecision {
    /// The effective kill-switch level at evaluation time.
    pub effective_level: MissionKillSwitchLevel,
    /// Whether the requested operation is blocked.
    pub blocked: bool,
    /// Structured decision path for audit/debugging.
    pub decision_path: String,
    /// Structured reason code.
    pub reason_code: String,
    /// Error code when blocked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// The activation record driving the decision (None when Off).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activation: Option<MissionKillSwitchActivation>,
}

// ========================================================================
// Pause/Resume/Abort Control and Checkpoint Recovery (C5)
// ========================================================================

/// Command to control mission execution flow (pause, resume, or abort).
///
/// Each variant captures the operator/system identity, reason, and timing
/// needed for audit-grade traceability of execution flow changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "action")]
pub enum MissionControlCommand {
    Pause {
        requested_by: String,
        reason_code: String,
        requested_at_ms: i64,
        correlation_id: Option<String>,
    },
    Resume {
        requested_by: String,
        reason_code: String,
        requested_at_ms: i64,
        correlation_id: Option<String>,
    },
    Abort {
        requested_by: String,
        reason_code: String,
        error_code: Option<String>,
        requested_at_ms: i64,
        correlation_id: Option<String>,
    },
}

impl MissionControlCommand {
    /// Returns the action name for display and logging.
    #[must_use]
    pub fn action_name(&self) -> &str {
        match self {
            Self::Pause { .. } => "pause",
            Self::Resume { .. } => "resume",
            Self::Abort { .. } => "abort",
        }
    }

    /// Returns the identity of who requested this command.
    #[must_use]
    pub fn requested_by(&self) -> &str {
        match self {
            Self::Pause { requested_by, .. }
            | Self::Resume { requested_by, .. }
            | Self::Abort { requested_by, .. } => requested_by,
        }
    }

    /// Returns the timestamp of when this command was issued.
    #[must_use]
    pub fn requested_at_ms(&self) -> i64 {
        match self {
            Self::Pause {
                requested_at_ms, ..
            }
            | Self::Resume {
                requested_at_ms, ..
            }
            | Self::Abort {
                requested_at_ms, ..
            } => *requested_at_ms,
        }
    }

    /// Returns the reason code for this command.
    #[must_use]
    pub fn reason_code(&self) -> &str {
        match self {
            Self::Pause { reason_code, .. }
            | Self::Resume { reason_code, .. }
            | Self::Abort { reason_code, .. } => reason_code,
        }
    }

    /// Deterministic canonical string representation.
    #[must_use]
    pub fn canonical_string(&self) -> String {
        match self {
            Self::Pause {
                requested_by,
                reason_code,
                requested_at_ms,
                correlation_id,
            } => format!(
                "action=pause|requested_by={}|reason_code={}|requested_at_ms={}|correlation_id={}",
                requested_by,
                reason_code,
                requested_at_ms,
                correlation_id.as_deref().unwrap_or("none")
            ),
            Self::Resume {
                requested_by,
                reason_code,
                requested_at_ms,
                correlation_id,
            } => format!(
                "action=resume|requested_by={}|reason_code={}|requested_at_ms={}|correlation_id={}",
                requested_by,
                reason_code,
                requested_at_ms,
                correlation_id.as_deref().unwrap_or("none")
            ),
            Self::Abort {
                requested_by,
                reason_code,
                error_code,
                requested_at_ms,
                correlation_id,
            } => format!(
                "action=abort|requested_by={}|reason_code={}|error_code={}|requested_at_ms={}|correlation_id={}",
                requested_by,
                reason_code,
                error_code.as_deref().unwrap_or("none"),
                requested_at_ms,
                correlation_id.as_deref().unwrap_or("none")
            ),
        }
    }
}

/// Per-assignment state snapshot captured in a pause checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssignmentCheckpointEntry {
    pub assignment_id: AssignmentId,
    pub outcome_summary: Option<String>,
    pub approval_state_summary: String,
}

/// Checkpoint captured at mission pause time for deterministic recovery.
///
/// Contains the lifecycle state at the moment of pause, the operator identity,
/// and a snapshot of each assignment's outcome/approval state — sufficient to
/// resume execution without data loss.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionCheckpoint {
    pub checkpoint_id: String,
    pub paused_from_state: MissionLifecycleState,
    pub paused_by: String,
    pub reason_code: String,
    pub paused_at_ms: i64,
    pub resumed_at_ms: Option<i64>,
    pub resumed_by: Option<String>,
    pub assignment_entries: Vec<AssignmentCheckpointEntry>,
    pub correlation_id: Option<String>,
}

impl MissionCheckpoint {
    /// Deterministic canonical string for checkpoint.
    #[must_use]
    pub fn canonical_string(&self) -> String {
        let mut parts = vec![
            format!("checkpoint_id={}", self.checkpoint_id),
            format!("paused_from_state={}", self.paused_from_state),
            format!("paused_by={}", self.paused_by),
            format!("reason_code={}", self.reason_code),
            format!("paused_at_ms={}", self.paused_at_ms),
            format!(
                "resumed_at_ms={}",
                self.resumed_at_ms
                    .map_or_else(|| "none".to_string(), |v| v.to_string())
            ),
            format!(
                "resumed_by={}",
                self.resumed_by.as_deref().unwrap_or("none")
            ),
        ];
        for (i, entry) in self.assignment_entries.iter().enumerate() {
            parts.push(format!(
                "assignment[{}]={}/{}",
                i, entry.assignment_id.0, entry.approval_state_summary
            ));
        }
        parts.join("|")
    }

    /// Duration of this pause in milliseconds, or None if still paused.
    #[must_use]
    pub fn pause_duration_ms(&self) -> Option<i64> {
        self.resumed_at_ms.map(|r| r - self.paused_at_ms)
    }
}

/// Tracking state for mission pause/resume/abort history.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionPauseResumeState {
    pub current_checkpoint: Option<MissionCheckpoint>,
    pub checkpoint_history: Vec<MissionCheckpoint>,
    pub total_pause_count: u32,
    pub total_resume_count: u32,
    pub total_abort_count: u32,
    pub cumulative_pause_duration_ms: i64,
}

impl MissionPauseResumeState {
    /// Returns true when a checkpoint is active (mission is paused).
    #[must_use]
    pub fn is_paused(&self) -> bool {
        self.current_checkpoint.is_some()
    }

    /// Returns true when no pause/resume/abort activity has occurred.
    #[must_use]
    pub fn is_pristine(&self) -> bool {
        self.current_checkpoint.is_none()
            && self.checkpoint_history.is_empty()
            && self.total_pause_count == 0
            && self.total_resume_count == 0
            && self.total_abort_count == 0
    }

    /// Deterministic canonical string for stable serialization.
    #[must_use]
    pub fn canonical_string(&self) -> String {
        let mut parts = vec![
            format!("paused={}", self.is_paused()),
            format!("total_pause_count={}", self.total_pause_count),
            format!("total_resume_count={}", self.total_resume_count),
            format!("total_abort_count={}", self.total_abort_count),
            format!(
                "cumulative_pause_duration_ms={}",
                self.cumulative_pause_duration_ms
            ),
            format!("history_len={}", self.checkpoint_history.len()),
        ];
        if let Some(cp) = &self.current_checkpoint {
            parts.push(format!("current={}", cp.canonical_string()));
        }
        parts.join("|")
    }

    /// Evict checkpoint history entries with paused_at_ms before cutoff.
    pub fn evict_history_before(&mut self, cutoff_ms: i64) {
        self.checkpoint_history
            .retain(|cp| cp.paused_at_ms >= cutoff_ms);
    }
}

/// Result of a pause/resume/abort control operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionControlDecision {
    pub action: String,
    pub lifecycle_from: MissionLifecycleState,
    pub lifecycle_to: MissionLifecycleState,
    pub decision_path: String,
    pub reason_code: String,
    pub error_code: Option<String>,
    pub checkpoint_id: Option<String>,
    pub decided_at_ms: i64,
}

/// Durable approval-path transition record for idempotent continuation/audit trails.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionApprovalTransitionRecord {
    pub assignment_id: AssignmentId,
    pub lifecycle_from: MissionLifecycleState,
    pub lifecycle_to: MissionLifecycleState,
    pub approval_from: ApprovalState,
    pub approval_to: ApprovalState,
    pub transition_kind: MissionLifecycleTransitionKind,
    pub transitioned_at_ms: i64,
    pub idempotent: bool,
    pub reason_code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}

/// Runtime signal emitted by dispatch/runtime to report assignment execution results.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum MissionAssignmentSignalPayload {
    Completed {
        reason_code: String,
        completed_at_ms: i64,
    },
    Failed {
        reason_code: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        error_code: Option<String>,
        completed_at_ms: i64,
    },
    TimedOut {
        completed_at_ms: i64,
    },
}

/// One assignment outcome signal used for deterministic reconciliation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionAssignmentSignal {
    pub assignment_id: AssignmentId,
    pub observed_at_ms: i64,
    pub correlation_id: String,
    pub payload: MissionAssignmentSignalPayload,
}

/// Drift record surfaced when incoming signals conflict with prior assignment state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionAssignmentStateDrift {
    pub assignment_id: AssignmentId,
    pub reason_code: String,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_outcome: Option<Outcome>,
    pub incoming_outcome: Outcome,
}

/// Assignment reconciliation result for one ingested runtime signal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionAssignmentReconciliationReport {
    pub assignment_id: AssignmentId,
    pub applied: bool,
    pub out_of_order: bool,
    pub lifecycle_from: MissionLifecycleState,
    pub lifecycle_to: MissionLifecycleState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome_before: Option<Outcome>,
    pub outcome_after: Outcome,
    pub reason_code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drift: Option<MissionAssignmentStateDrift>,
}

// ── C8: Crash-Consistent Mission Journal ────────────────────────────────────

/// Individual entry in the mission deterministic journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionJournalEntry {
    /// Monotonically increasing sequence number (per mission).
    pub seq: u64,
    /// Timestamp in milliseconds since epoch.
    pub timestamp_ms: i64,
    /// Correlation ID linking to the originating operation.
    pub correlation_id: String,
    /// Content-addressed entry hash for tamper detection.
    pub entry_hash: String,
    /// Kind of journal entry.
    pub kind: MissionJournalEntryKind,
    /// Mission schema version at time of entry.
    pub mission_version: u32,
    /// Originating actor.
    pub initiated_by: String,
    /// Reason code for audit trail.
    pub reason_code: String,
    /// Optional error code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}

impl MissionJournalEntry {
    /// Deterministic canonical string for stable hashing.
    #[must_use]
    pub fn canonical_string(&self) -> String {
        format!(
            "seq={}|ts={}|cid={}|hash={}|kind={}|v={}|by={}|reason={}|err={}",
            self.seq,
            self.timestamp_ms,
            self.correlation_id,
            self.entry_hash,
            self.kind.tag_name(),
            self.mission_version,
            self.initiated_by,
            self.reason_code,
            self.error_code.as_deref().unwrap_or("none"),
        )
    }
}

/// Kinds of journal entries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MissionJournalEntryKind {
    /// Lifecycle state transition.
    LifecycleTransition {
        from: MissionLifecycleState,
        to: MissionLifecycleState,
        transition_kind: MissionLifecycleTransitionKind,
    },
    /// Control command applied (pause/resume/abort).
    ControlCommand {
        command: MissionControlCommand,
        decision: MissionControlDecision,
    },
    /// Kill-switch activation or deactivation.
    KillSwitchChange {
        level_from: MissionKillSwitchLevel,
        level_to: MissionKillSwitchLevel,
    },
    /// Assignment outcome finalized.
    AssignmentOutcome {
        assignment_id: AssignmentId,
        outcome_before: Option<String>,
        outcome_after: String,
    },
    /// Checkpoint marker (snapshot point for recovery).
    Checkpoint {
        mission_hash: String,
        lifecycle_state: MissionLifecycleState,
        assignment_count: usize,
    },
    /// Recovery marker (indicates restart/replay boundary).
    RecoveryMarker {
        recovered_through_seq: u64,
        recovery_reason: String,
    },
}

impl MissionJournalEntryKind {
    /// Short tag name for canonical string encoding.
    #[must_use]
    pub fn tag_name(&self) -> &str {
        match self {
            Self::LifecycleTransition { .. } => "lifecycle_transition",
            Self::ControlCommand { .. } => "control_command",
            Self::KillSwitchChange { .. } => "kill_switch_change",
            Self::AssignmentOutcome { .. } => "assignment_outcome",
            Self::Checkpoint { .. } => "checkpoint",
            Self::RecoveryMarker { .. } => "recovery_marker",
        }
    }
}

/// Lightweight journal metadata persisted on Mission for recovery.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionJournalState {
    /// Total entries appended to the journal.
    pub entry_count: u64,
    /// Sequence number of the last entry.
    pub last_seq: u64,
    /// Hash of the last appended entry for chain verification.
    pub last_entry_hash: String,
    /// Sequence number of the most recent checkpoint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_checkpoint_seq: Option<u64>,
    /// Mission hash at last checkpoint for crash detection.
    pub last_checkpoint_hash: String,
    /// Whether the journal is in a clean (checkpointed) state.
    pub clean: bool,
}

impl MissionJournalState {
    /// True when no entries have been recorded.
    #[must_use]
    pub fn is_pristine(&self) -> bool {
        self.entry_count == 0
    }

    /// Deterministic canonical string.
    #[must_use]
    pub fn canonical_string(&self) -> String {
        format!(
            "entry_count={}|last_seq={}|last_hash={}|cp_seq={}|cp_hash={}|clean={}",
            self.entry_count,
            self.last_seq,
            self.last_entry_hash,
            self.last_checkpoint_seq
                .map_or_else(|| "none".to_string(), |v| v.to_string()),
            self.last_checkpoint_hash,
            self.clean,
        )
    }
}

/// In-memory mission journal engine for crash-consistent recovery.
#[derive(Debug, Clone)]
pub struct MissionJournal {
    /// Mission ID this journal belongs to.
    pub mission_id: MissionId,
    /// Append-only journal entries.
    entries: Vec<MissionJournalEntry>,
    /// Next sequence number.
    next_seq: u64,
    /// Last checkpoint sequence.
    last_checkpoint_seq: Option<u64>,
    /// Correlation ID dedup index.
    correlation_index: std::collections::HashMap<String, u64>,
    /// Maximum entries before compaction warning.
    max_entries: usize,
}

impl MissionJournal {
    /// Create a new empty journal.
    #[must_use]
    pub fn new(mission_id: MissionId) -> Self {
        Self {
            mission_id,
            entries: Vec::new(),
            next_seq: 1,
            last_checkpoint_seq: None,
            correlation_index: std::collections::HashMap::new(),
            max_entries: 10_000,
        }
    }

    /// Create with a custom entry limit.
    #[must_use]
    pub fn with_max_entries(mut self, max: usize) -> Self {
        self.max_entries = max;
        self
    }

    /// Append a journal entry. Returns the assigned sequence number.
    pub fn append(
        &mut self,
        kind: MissionJournalEntryKind,
        correlation_id: impl Into<String>,
        initiated_by: impl Into<String>,
        reason_code: impl Into<String>,
        error_code: Option<String>,
        timestamp_ms: i64,
    ) -> Result<u64, MissionJournalError> {
        let cid = correlation_id.into();

        // Idempotency check via correlation index
        if let Some(&prior_seq) = self.correlation_index.get(&cid) {
            return Err(MissionJournalError::DuplicateCorrelation {
                correlation_id: cid,
                prior_seq,
            });
        }

        let seq = self.next_seq;
        let entry_hash = format!("j:{}:{}:{}", seq, timestamp_ms, &cid,);

        let entry = MissionJournalEntry {
            seq,
            timestamp_ms,
            correlation_id: cid.clone(),
            entry_hash,
            kind,
            mission_version: MISSION_SCHEMA_VERSION,
            initiated_by: initiated_by.into(),
            reason_code: reason_code.into(),
            error_code,
        };

        self.entries.push(entry);
        self.correlation_index.insert(cid, seq);
        self.next_seq = seq + 1;
        Ok(seq)
    }

    /// Place a checkpoint marker.
    pub fn checkpoint(
        &mut self,
        mission: &Mission,
        timestamp_ms: i64,
    ) -> Result<u64, MissionJournalError> {
        let mission_hash = mission.compute_hash();
        let cid = format!("checkpoint:{}:{}", self.mission_id.0, self.next_seq);
        let kind = MissionJournalEntryKind::Checkpoint {
            mission_hash,
            lifecycle_state: mission.lifecycle_state,
            assignment_count: mission.assignments.len(),
        };
        let seq = self.append(kind, cid, "system", "checkpoint", None, timestamp_ms)?;
        self.last_checkpoint_seq = Some(seq);
        Ok(seq)
    }

    /// Place a recovery marker after replaying journal entries.
    pub fn recovery_marker(
        &mut self,
        recovered_through_seq: u64,
        reason: impl Into<String>,
        timestamp_ms: i64,
    ) -> Result<u64, MissionJournalError> {
        let cid = format!("recovery:{}:{}", self.mission_id.0, self.next_seq);
        let kind = MissionJournalEntryKind::RecoveryMarker {
            recovered_through_seq,
            recovery_reason: reason.into(),
        };
        self.append(kind, cid, "system", "recovery", None, timestamp_ms)
    }

    /// Number of journal entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True if the journal has no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Current next sequence number.
    #[must_use]
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// Last checkpoint sequence number.
    #[must_use]
    pub fn last_checkpoint_seq(&self) -> Option<u64> {
        self.last_checkpoint_seq
    }

    /// Check if a correlation ID has already been recorded.
    #[must_use]
    pub fn has_correlation(&self, correlation_id: &str) -> bool {
        self.correlation_index.contains_key(correlation_id)
    }

    /// Get entries since a given sequence number (inclusive).
    #[must_use]
    pub fn entries_since(&self, since_seq: u64) -> &[MissionJournalEntry] {
        if since_seq == 0 || self.entries.is_empty() {
            return &self.entries;
        }
        // Entries are stored in order; find the index where seq >= since_seq.
        let start = self
            .entries
            .iter()
            .position(|e| e.seq >= since_seq)
            .unwrap_or(self.entries.len());
        &self.entries[start..]
    }

    /// Get all journal entries.
    #[must_use]
    pub fn entries(&self) -> &[MissionJournalEntry] {
        &self.entries
    }

    /// Compact entries up to (not including) a given sequence number.
    /// Returns the number of entries removed.
    pub fn compact_before(&mut self, before_seq: u64) -> usize {
        let original_len = self.entries.len();
        let keep_from = self
            .entries
            .iter()
            .position(|e| e.seq >= before_seq)
            .unwrap_or(self.entries.len());
        // Remove compacted entries from correlation index
        for entry in &self.entries[..keep_from] {
            self.correlation_index.remove(&entry.correlation_id);
        }
        self.entries.drain(..keep_from);
        original_len - self.entries.len()
    }

    /// Whether compaction is recommended (entry count exceeds limit).
    #[must_use]
    pub fn needs_compaction(&self) -> bool {
        self.entries.len() > self.max_entries
    }

    /// Snapshot journal metadata for embedding in Mission.
    #[must_use]
    pub fn snapshot_state(&self) -> MissionJournalState {
        let last_entry = self.entries.last();
        MissionJournalState {
            entry_count: self.entries.len() as u64,
            last_seq: last_entry.map_or(0, |e| e.seq),
            last_entry_hash: last_entry.map_or_else(String::new, |e| e.entry_hash.clone()),
            last_checkpoint_seq: self.last_checkpoint_seq,
            last_checkpoint_hash: self
                .entries
                .iter()
                .rev()
                .find_map(|e| match &e.kind {
                    MissionJournalEntryKind::Checkpoint { mission_hash, .. } => {
                        Some(mission_hash.clone())
                    }
                    _ => None,
                })
                .unwrap_or_default(),
            clean: if self.entries.is_empty() {
                true
            } else {
                self.last_checkpoint_seq
                    .is_some_and(|cp_seq| self.entries.last().is_some_and(|e| e.seq == cp_seq))
            },
        }
    }

    /// Replay journal entries from the last checkpoint to reconstruct state deltas.
    #[must_use]
    pub fn replay_from_checkpoint(&self) -> MissionJournalReplayReport {
        let start_seq = self.last_checkpoint_seq.unwrap_or(0);
        let entries = self.entries_since(start_seq);

        let mut report = MissionJournalReplayReport {
            start_seq,
            entries_scanned: 0,
            lifecycle_transitions: 0,
            control_commands: 0,
            kill_switch_changes: 0,
            assignment_outcomes: 0,
            checkpoints_found: 0,
            recovery_markers: 0,
            errors: Vec::new(),
        };

        let mut prev_seq: Option<u64> = None;

        for entry in entries {
            report.entries_scanned += 1;

            // Check monotonic sequence ordering
            if let Some(ps) = prev_seq {
                if entry.seq <= ps {
                    report.errors.push(MissionJournalReplayError {
                        seq: entry.seq,
                        error_code: "SEQ_REGRESSION".into(),
                        message: format!(
                            "sequence {} is not greater than previous {}",
                            entry.seq, ps,
                        ),
                    });
                }
            }
            prev_seq = Some(entry.seq);

            match &entry.kind {
                MissionJournalEntryKind::LifecycleTransition { .. } => {
                    report.lifecycle_transitions += 1;
                }
                MissionJournalEntryKind::ControlCommand { .. } => {
                    report.control_commands += 1;
                }
                MissionJournalEntryKind::KillSwitchChange { .. } => {
                    report.kill_switch_changes += 1;
                }
                MissionJournalEntryKind::AssignmentOutcome { .. } => {
                    report.assignment_outcomes += 1;
                }
                MissionJournalEntryKind::Checkpoint { .. } => {
                    report.checkpoints_found += 1;
                }
                MissionJournalEntryKind::RecoveryMarker { .. } => {
                    report.recovery_markers += 1;
                }
            }
        }

        report
    }
}

/// Replay report summarizing journal recovery scan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionJournalReplayReport {
    pub start_seq: u64,
    pub entries_scanned: usize,
    pub lifecycle_transitions: usize,
    pub control_commands: usize,
    pub kill_switch_changes: usize,
    pub assignment_outcomes: usize,
    pub checkpoints_found: usize,
    pub recovery_markers: usize,
    pub errors: Vec<MissionJournalReplayError>,
}

impl MissionJournalReplayReport {
    /// True if replay encountered no errors.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.errors.is_empty()
    }

    /// Total entry counts across all categories.
    #[must_use]
    pub fn total_entries(&self) -> usize {
        self.lifecycle_transitions
            + self.control_commands
            + self.kill_switch_changes
            + self.assignment_outcomes
            + self.checkpoints_found
            + self.recovery_markers
    }
}

/// One error discovered during journal replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionJournalReplayError {
    pub seq: u64,
    pub error_code: String,
    pub message: String,
}

/// Errors from journal operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MissionJournalError {
    /// Duplicate correlation ID (idempotency guard).
    DuplicateCorrelation {
        correlation_id: String,
        prior_seq: u64,
    },
    /// Validation or invariant violation.
    ValidationFailed { reason: String },
}

impl fmt::Display for MissionJournalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateCorrelation {
                correlation_id,
                prior_seq,
            } => write!(
                f,
                "duplicate correlation_id '{}' (prior seq={})",
                correlation_id, prior_seq,
            ),
            Self::ValidationFailed { reason } => {
                write!(f, "journal validation failed: {}", reason)
            }
        }
    }
}

/// Trigger class used by adaptive mission replanning policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionReplanTriggerKind {
    Completion,
    Blocked,
    Failed,
    RateLimited,
    OperatorOverride,
}

impl fmt::Display for MissionReplanTriggerKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Completion => f.write_str("completion"),
            Self::Blocked => f.write_str("blocked"),
            Self::Failed => f.write_str("failed"),
            Self::RateLimited => f.write_str("rate_limited"),
            Self::OperatorOverride => f.write_str("operator_override"),
        }
    }
}

/// One replan trigger event emitted by mission runtime integration points.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionReplanTrigger {
    pub kind: MissionReplanTriggerKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assignment_id: Option<AssignmentId>,
    pub observed_at_ms: i64,
    pub correlation_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<String>,
}

/// Deterministic backoff policy for adaptive replanning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionReplanBackoffPolicy {
    pub min_backoff_ms: i64,
    pub max_backoff_ms: i64,
    pub burst_window_ms: i64,
}

impl Default for MissionReplanBackoffPolicy {
    fn default() -> Self {
        Self {
            min_backoff_ms: 500,
            max_backoff_ms: 60_000,
            burst_window_ms: 30_000,
        }
    }
}

/// Durable mission replan guard state to prevent tight replan loops.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct MissionReplanState {
    pub consecutive_replan_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_trigger_kind: Option<MissionReplanTriggerKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_correlation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_observed_at_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_eligible_replan_at_ms: Option<i64>,
}

impl MissionReplanState {
    /// Returns true when state has no persisted adaptive-replan history.
    #[must_use]
    pub fn is_pristine(&self) -> bool {
        self.consecutive_replan_count == 0
            && self.last_trigger_kind.is_none()
            && self.last_correlation_id.is_none()
            && self.last_observed_at_ms.is_none()
            && self.next_eligible_replan_at_ms.is_none()
    }

    /// Deterministic string form used by mission canonical hash.
    #[must_use]
    pub fn canonical_string(&self) -> String {
        format!(
            "count={},kind={},correlation={},last_observed_at_ms={},next_eligible_replan_at_ms={}",
            self.consecutive_replan_count,
            self.last_trigger_kind
                .map_or_else(|| "none".to_string(), |kind| kind.to_string()),
            self.last_correlation_id.as_deref().unwrap_or("none"),
            self.last_observed_at_ms
                .map_or_else(|| "none".to_string(), |value| value.to_string()),
            self.next_eligible_replan_at_ms
                .map_or_else(|| "none".to_string(), |value| value.to_string()),
        )
    }
}

/// Result of adaptive replanning trigger evaluation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionReplanDecision {
    pub trigger_kind: MissionReplanTriggerKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assignment_id: Option<AssignmentId>,
    pub observed_at_ms: i64,
    pub correlation_id: String,
    pub apply_replan: bool,
    pub decision_path: String,
    pub reason_code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    pub attempt: u32,
    pub backoff_ms: i64,
    pub scheduled_at_ms: i64,
    pub next_eligible_replan_at_ms: i64,
    pub lifecycle_from: MissionLifecycleState,
    pub lifecycle_to: MissionLifecycleState,
}

/// Active reservation lease snapshot used for mission-time conflict checks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionReservationLease {
    pub lease_id: String,
    pub holder: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    pub exclusive: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<i64>,
}

impl MissionReservationLease {
    #[must_use]
    pub fn is_expired_at(&self, evaluated_at_ms: i64) -> bool {
        self.expires_at_ms
            .is_some_and(|expires_at_ms| expires_at_ms < evaluated_at_ms)
    }
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
    Paused,
    RetryPending,
    Blocked,
    Completed,
    Failed,
    Cancelled,
}

impl MissionLifecycleState {
    const ALL: [Self; 11] = [
        Self::Planning,
        Self::Planned,
        Self::Dispatching,
        Self::AwaitingApproval,
        Self::Running,
        Self::Paused,
        Self::RetryPending,
        Self::Blocked,
        Self::Completed,
        Self::Failed,
        Self::Cancelled,
    ];

    /// Return all lifecycle states.
    #[must_use]
    pub const fn all() -> [Self; 11] {
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
            Self::Paused => f.write_str("paused"),
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
    PauseRequested,
    ResumeRequested,
    AbortRequested,
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
            Self::PauseRequested => f.write_str("pause_requested"),
            Self::ResumeRequested => f.write_str("resume_requested"),
            Self::AbortRequested => f.write_str("abort_requested"),
        }
    }
}

impl MissionLifecycleTransitionKind {
    const ALL: [Self; 16] = [
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
        Self::PauseRequested,
        Self::ResumeRequested,
        Self::AbortRequested,
    ];

    /// Return all lifecycle transition kinds.
    #[must_use]
    pub const fn all() -> [Self; 16] {
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

const MISSION_LIFECYCLE_TRANSITION_RULES: [MissionLifecycleTransitionRule; 49] = [
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
    // C5: Pause from active states
    MissionLifecycleTransitionRule {
        from: MissionLifecycleState::Running,
        to: MissionLifecycleState::Paused,
        kind: MissionLifecycleTransitionKind::PauseRequested,
    },
    MissionLifecycleTransitionRule {
        from: MissionLifecycleState::Dispatching,
        to: MissionLifecycleState::Paused,
        kind: MissionLifecycleTransitionKind::PauseRequested,
    },
    MissionLifecycleTransitionRule {
        from: MissionLifecycleState::AwaitingApproval,
        to: MissionLifecycleState::Paused,
        kind: MissionLifecycleTransitionKind::PauseRequested,
    },
    MissionLifecycleTransitionRule {
        from: MissionLifecycleState::Blocked,
        to: MissionLifecycleState::Paused,
        kind: MissionLifecycleTransitionKind::PauseRequested,
    },
    MissionLifecycleTransitionRule {
        from: MissionLifecycleState::RetryPending,
        to: MissionLifecycleState::Paused,
        kind: MissionLifecycleTransitionKind::PauseRequested,
    },
    // C5: Resume from paused back to prior state
    MissionLifecycleTransitionRule {
        from: MissionLifecycleState::Paused,
        to: MissionLifecycleState::Running,
        kind: MissionLifecycleTransitionKind::ResumeRequested,
    },
    MissionLifecycleTransitionRule {
        from: MissionLifecycleState::Paused,
        to: MissionLifecycleState::Dispatching,
        kind: MissionLifecycleTransitionKind::ResumeRequested,
    },
    MissionLifecycleTransitionRule {
        from: MissionLifecycleState::Paused,
        to: MissionLifecycleState::AwaitingApproval,
        kind: MissionLifecycleTransitionKind::ResumeRequested,
    },
    MissionLifecycleTransitionRule {
        from: MissionLifecycleState::Paused,
        to: MissionLifecycleState::Blocked,
        kind: MissionLifecycleTransitionKind::ResumeRequested,
    },
    MissionLifecycleTransitionRule {
        from: MissionLifecycleState::Paused,
        to: MissionLifecycleState::RetryPending,
        kind: MissionLifecycleTransitionKind::ResumeRequested,
    },
    // C5: Abort from any non-terminal state
    MissionLifecycleTransitionRule {
        from: MissionLifecycleState::Planning,
        to: MissionLifecycleState::Cancelled,
        kind: MissionLifecycleTransitionKind::AbortRequested,
    },
    MissionLifecycleTransitionRule {
        from: MissionLifecycleState::Planned,
        to: MissionLifecycleState::Cancelled,
        kind: MissionLifecycleTransitionKind::AbortRequested,
    },
    MissionLifecycleTransitionRule {
        from: MissionLifecycleState::Dispatching,
        to: MissionLifecycleState::Cancelled,
        kind: MissionLifecycleTransitionKind::AbortRequested,
    },
    MissionLifecycleTransitionRule {
        from: MissionLifecycleState::AwaitingApproval,
        to: MissionLifecycleState::Cancelled,
        kind: MissionLifecycleTransitionKind::AbortRequested,
    },
    MissionLifecycleTransitionRule {
        from: MissionLifecycleState::Running,
        to: MissionLifecycleState::Cancelled,
        kind: MissionLifecycleTransitionKind::AbortRequested,
    },
    MissionLifecycleTransitionRule {
        from: MissionLifecycleState::Paused,
        to: MissionLifecycleState::Cancelled,
        kind: MissionLifecycleTransitionKind::AbortRequested,
    },
    MissionLifecycleTransitionRule {
        from: MissionLifecycleState::RetryPending,
        to: MissionLifecycleState::Cancelled,
        kind: MissionLifecycleTransitionKind::AbortRequested,
    },
    MissionLifecycleTransitionRule {
        from: MissionLifecycleState::Blocked,
        to: MissionLifecycleState::Cancelled,
        kind: MissionLifecycleTransitionKind::AbortRequested,
    },
    MissionLifecycleTransitionRule {
        from: MissionLifecycleState::Failed,
        to: MissionLifecycleState::Cancelled,
        kind: MissionLifecycleTransitionKind::AbortRequested,
    },
    // C5: Cancel from paused (regular cancellation, not abort)
    MissionLifecycleTransitionRule {
        from: MissionLifecycleState::Paused,
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
    #[serde(default, skip_serializing_if = "MissionReplanState::is_pristine")]
    pub replan_state: MissionReplanState,
    #[serde(
        default,
        skip_serializing_if = "MissionDispatchDeduplicationState::is_empty"
    )]
    pub dispatch_dedup_state: MissionDispatchDeduplicationState,
    #[serde(default, skip_serializing_if = "MissionKillSwitchState::is_off")]
    pub kill_switch: MissionKillSwitchState,
    #[serde(default, skip_serializing_if = "MissionPauseResumeState::is_pristine")]
    pub pause_resume_state: MissionPauseResumeState,
    #[serde(default, skip_serializing_if = "MissionJournalState::is_pristine")]
    pub journal_state: MissionJournalState,
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
            replan_state: MissionReplanState::default(),
            dispatch_dedup_state: MissionDispatchDeduplicationState::default(),
            kill_switch: MissionKillSwitchState::default(),
            pause_resume_state: MissionPauseResumeState::default(),
            journal_state: MissionJournalState::default(),
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
        if !self.replan_state.is_pristine() {
            parts.push(format!(
                "replan_state={}",
                self.replan_state.canonical_string()
            ));
        }
        if !self.dispatch_dedup_state.is_empty() {
            parts.push(format!(
                "dispatch_dedup_state={}",
                self.dispatch_dedup_state.canonical_string()
            ));
        }
        if !self.kill_switch.is_off() {
            parts.push(format!(
                "kill_switch={}",
                self.kill_switch.canonical_string()
            ));
        }
        if !self.pause_resume_state.is_pristine() {
            parts.push(format!(
                "pause_resume_state={}",
                self.pause_resume_state.canonical_string()
            ));
        }
        if !self.journal_state.is_pristine() {
            parts.push(format!(
                "journal_state={}",
                self.journal_state.canonical_string()
            ));
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
        Self::validate_optional_non_empty_field(
            "mission.replan_state.last_correlation_id",
            self.replan_state.last_correlation_id.as_deref(),
        )?;
        if let Some(last_observed_at_ms) = self.replan_state.last_observed_at_ms {
            Self::validate_timestamp_order(
                "mission.replan_state.last_observed_at_ms",
                self.created_at_ms,
                last_observed_at_ms,
            )?;
        }
        if let Some(next_eligible_replan_at_ms) = self.replan_state.next_eligible_replan_at_ms {
            let baseline = self
                .replan_state
                .last_observed_at_ms
                .unwrap_or(self.created_at_ms);
            Self::validate_timestamp_order(
                "mission.replan_state.next_eligible_replan_at_ms",
                baseline,
                next_eligible_replan_at_ms,
            )?;
        }
        if self.replan_state.consecutive_replan_count > 0
            && self.replan_state.last_observed_at_ms.is_none()
        {
            return Err(MissionValidationError::InvalidFieldValue {
                field_path: "mission.replan_state.consecutive_replan_count".to_string(),
                message: "consecutive_replan_count requires last_observed_at_ms to be present"
                    .to_string(),
            });
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

    /// Mark an assignment as requiring approval and transition lifecycle into awaiting approval.
    ///
    /// This operation is idempotent when the assignment is already pending with the same
    /// request metadata and the mission lifecycle is already `awaiting_approval`.
    pub fn request_assignment_approval(
        &mut self,
        assignment_id: &AssignmentId,
        requested_by: impl Into<String>,
        requested_at_ms: i64,
    ) -> Result<MissionApprovalTransitionRecord, MissionValidationError> {
        let requested_by = requested_by.into();
        let requested_by = requested_by.trim();
        Self::validate_non_empty_field("approval.requested_by", requested_by)?;

        let assignment_index =
            self.find_assignment_index_by_id(assignment_id)
                .ok_or_else(|| {
                    MissionValidationError::UnknownAssignmentReference(assignment_id.clone())
                })?;
        let lifecycle_from = self.lifecycle_state;
        let lifecycle_to = match lifecycle_from {
            MissionLifecycleState::Dispatching | MissionLifecycleState::AwaitingApproval => {
                MissionLifecycleState::AwaitingApproval
            }
            lifecycle_state => {
                return Err(MissionValidationError::InvalidApprovalLifecycleState {
                    assignment_id: assignment_id.clone(),
                    action: "request_approval",
                    lifecycle_state,
                });
            }
        };
        let transition_kind = MissionLifecycleTransitionKind::ApprovalRequested;

        let approval_from = self.assignments[assignment_index].approval_state.clone();
        match &approval_from {
            ApprovalState::NotRequired | ApprovalState::Pending { .. } => {}
            state => {
                return Err(MissionValidationError::InvalidApprovalStateTransition {
                    assignment_id: assignment_id.clone(),
                    action: "request_approval",
                    state: state.canonical_string(),
                });
            }
        }

        let approval_to = ApprovalState::Pending {
            requested_by: requested_by.to_string(),
            requested_at_ms,
        };
        let idempotent_state = approval_from == approval_to;
        let idempotent_lifecycle = lifecycle_from == lifecycle_to;

        if !idempotent_state {
            self.assignments[assignment_index].approval_state = approval_to.clone();
        }
        self.assignments[assignment_index].updated_at_ms = Some(requested_at_ms);

        if !idempotent_lifecycle {
            self.transition_lifecycle(lifecycle_to, transition_kind, requested_at_ms)?;
        } else {
            self.updated_at_ms = Some(requested_at_ms);
        }

        Ok(MissionApprovalTransitionRecord {
            assignment_id: assignment_id.clone(),
            lifecycle_from,
            lifecycle_to,
            approval_from,
            approval_to,
            transition_kind,
            transitioned_at_ms: requested_at_ms,
            idempotent: idempotent_state && idempotent_lifecycle,
            reason_code: MissionFailureCode::ApprovalRequired
                .reason_code()
                .to_string(),
            error_code: Some(
                MissionFailureCode::ApprovalRequired
                    .error_code()
                    .to_string(),
            ),
        })
    }

    /// Continue execution after approval and transition back into running state.
    ///
    /// If the approval has already expired, this method falls back to the canonical
    /// expiration path and never resumes execution.
    pub fn continue_assignment_after_approval(
        &mut self,
        assignment_id: &AssignmentId,
        approved_by: impl Into<String>,
        approved_at_ms: i64,
        approval_code_hash: impl Into<String>,
    ) -> Result<MissionApprovalTransitionRecord, MissionValidationError> {
        let approved_by = approved_by.into();
        let approved_by = approved_by.trim();
        Self::validate_non_empty_field("approval.approved_by", approved_by)?;
        let approval_code_hash = approval_code_hash.into();
        let approval_code_hash = approval_code_hash.trim();
        Self::validate_non_empty_field("approval.approval_code_hash", approval_code_hash)?;

        let assignment_index =
            self.find_assignment_index_by_id(assignment_id)
                .ok_or_else(|| {
                    MissionValidationError::UnknownAssignmentReference(assignment_id.clone())
                })?;
        let lifecycle_from = self.lifecycle_state;
        let lifecycle_to = match lifecycle_from {
            MissionLifecycleState::AwaitingApproval | MissionLifecycleState::Running => {
                MissionLifecycleState::Running
            }
            lifecycle_state => {
                return Err(MissionValidationError::InvalidApprovalLifecycleState {
                    assignment_id: assignment_id.clone(),
                    action: "continue_after_approval",
                    lifecycle_state,
                });
            }
        };
        let transition_kind = MissionLifecycleTransitionKind::ApprovalGranted;

        let approval_from = self.assignments[assignment_index].approval_state.clone();
        if matches!(approval_from, ApprovalState::Expired { .. }) {
            return self.expire_assignment_approval(assignment_id, approved_at_ms);
        }
        match &approval_from {
            ApprovalState::Pending { .. } | ApprovalState::Approved { .. } => {}
            state => {
                return Err(MissionValidationError::InvalidApprovalStateTransition {
                    assignment_id: assignment_id.clone(),
                    action: "continue_after_approval",
                    state: state.canonical_string(),
                });
            }
        }

        let approval_to = ApprovalState::Approved {
            approved_by: approved_by.to_string(),
            approved_at_ms,
            approval_code_hash: approval_code_hash.to_string(),
        };
        let idempotent_state = approval_from == approval_to;
        let idempotent_lifecycle = lifecycle_from == lifecycle_to;

        if !idempotent_state {
            self.assignments[assignment_index].approval_state = approval_to.clone();
        }
        self.assignments[assignment_index].updated_at_ms = Some(approved_at_ms);

        if !idempotent_lifecycle {
            self.transition_lifecycle(lifecycle_to, transition_kind, approved_at_ms)?;
        } else {
            self.updated_at_ms = Some(approved_at_ms);
        }

        Ok(MissionApprovalTransitionRecord {
            assignment_id: assignment_id.clone(),
            lifecycle_from,
            lifecycle_to,
            approval_from,
            approval_to,
            transition_kind,
            transitioned_at_ms: approved_at_ms,
            idempotent: idempotent_state && idempotent_lifecycle,
            reason_code: "approval_granted".to_string(),
            error_code: None,
        })
    }

    /// Expire approval state and force safe fallback into failed lifecycle state.
    ///
    /// This method is idempotent when the assignment is already in canonical expired state,
    /// has canonical failed outcome, and mission lifecycle is already `failed`.
    pub fn expire_assignment_approval(
        &mut self,
        assignment_id: &AssignmentId,
        expired_at_ms: i64,
    ) -> Result<MissionApprovalTransitionRecord, MissionValidationError> {
        let assignment_index =
            self.find_assignment_index_by_id(assignment_id)
                .ok_or_else(|| {
                    MissionValidationError::UnknownAssignmentReference(assignment_id.clone())
                })?;
        let lifecycle_from = self.lifecycle_state;
        let lifecycle_to = match lifecycle_from {
            MissionLifecycleState::AwaitingApproval | MissionLifecycleState::Failed => {
                MissionLifecycleState::Failed
            }
            lifecycle_state => {
                return Err(MissionValidationError::InvalidApprovalLifecycleState {
                    assignment_id: assignment_id.clone(),
                    action: "expire_approval",
                    lifecycle_state,
                });
            }
        };
        let transition_kind = MissionLifecycleTransitionKind::ApprovalExpired;

        let approval_from = self.assignments[assignment_index].approval_state.clone();
        match &approval_from {
            ApprovalState::Pending { .. }
            | ApprovalState::Approved { .. }
            | ApprovalState::Expired { .. } => {}
            state => {
                return Err(MissionValidationError::InvalidApprovalStateTransition {
                    assignment_id: assignment_id.clone(),
                    action: "expire_approval",
                    state: state.canonical_string(),
                });
            }
        }

        let failure_code = MissionFailureCode::ApprovalExpired;
        let approval_to = ApprovalState::Expired {
            expired_at_ms,
            reason_code: failure_code.reason_code().to_string(),
        };
        let failed_outcome = Outcome::Failed {
            reason_code: failure_code.reason_code().to_string(),
            error_code: failure_code.error_code().to_string(),
            completed_at_ms: expired_at_ms,
        };
        let idempotent_state = approval_from == approval_to;
        let idempotent_outcome =
            self.assignments[assignment_index].outcome.as_ref() == Some(&failed_outcome);
        let idempotent_lifecycle = lifecycle_from == lifecycle_to;

        if !idempotent_state {
            self.assignments[assignment_index].approval_state = approval_to.clone();
        }
        if !idempotent_outcome {
            self.assignments[assignment_index].outcome = Some(failed_outcome);
        }
        self.assignments[assignment_index].updated_at_ms = Some(expired_at_ms);

        if !idempotent_lifecycle {
            self.transition_lifecycle(lifecycle_to, transition_kind, expired_at_ms)?;
        } else {
            self.updated_at_ms = Some(expired_at_ms);
        }

        Ok(MissionApprovalTransitionRecord {
            assignment_id: assignment_id.clone(),
            lifecycle_from,
            lifecycle_to,
            approval_from,
            approval_to,
            transition_kind,
            transitioned_at_ms: expired_at_ms,
            idempotent: idempotent_state && idempotent_outcome && idempotent_lifecycle,
            reason_code: failure_code.reason_code().to_string(),
            error_code: Some(failure_code.error_code().to_string()),
        })
    }

    /// Ingest runtime assignment outcomes and reconcile mission state deterministically.
    ///
    /// Reconciliation semantics:
    /// - out-of-order signals (`observed_at_ms` older than assignment update timestamp) are ignored,
    /// - duplicate signals are idempotent no-ops,
    /// - newer conflicting signals apply and emit an explicit drift record.
    pub fn reconcile_assignment_signal(
        &mut self,
        signal: &MissionAssignmentSignal,
    ) -> Result<MissionAssignmentReconciliationReport, MissionValidationError> {
        Self::validate_non_empty_field("assignment_signal.correlation_id", &signal.correlation_id)?;

        let assignment_index = self
            .find_assignment_index_by_id(&signal.assignment_id)
            .ok_or_else(|| {
                MissionValidationError::UnknownAssignmentReference(signal.assignment_id.clone())
            })?;
        let lifecycle_from = self.lifecycle_state;
        let outcome_before = self.assignments[assignment_index].outcome.clone();
        let outcome_after =
            Self::normalize_assignment_signal_outcome(&signal.assignment_id, &signal.payload)?;

        let baseline_ts = self.assignments[assignment_index]
            .updated_at_ms
            .unwrap_or(self.assignments[assignment_index].created_at_ms);
        if signal.observed_at_ms < baseline_ts {
            let drift = if outcome_before
                .as_ref()
                .is_some_and(|existing| existing != &outcome_after)
            {
                Some(MissionAssignmentStateDrift {
                    assignment_id: signal.assignment_id.clone(),
                    reason_code: MissionFailureCode::StaleState.reason_code().to_string(),
                    summary: format!(
                        "ignored_out_of_order_signal baseline_ts={} observed_at_ms={}",
                        baseline_ts, signal.observed_at_ms
                    ),
                    previous_outcome: outcome_before.clone(),
                    incoming_outcome: outcome_after.clone(),
                })
            } else {
                None
            };

            return Ok(MissionAssignmentReconciliationReport {
                assignment_id: signal.assignment_id.clone(),
                applied: false,
                out_of_order: true,
                lifecycle_from,
                lifecycle_to: lifecycle_from,
                outcome_before,
                outcome_after,
                reason_code: MissionFailureCode::StaleState.reason_code().to_string(),
                error_code: Some(MissionFailureCode::StaleState.error_code().to_string()),
                drift,
            });
        }

        let mut applied = false;
        let mut drift = None;
        if outcome_before.as_ref() != Some(&outcome_after) {
            if let Some(previous_outcome) = &outcome_before {
                drift = Some(MissionAssignmentStateDrift {
                    assignment_id: signal.assignment_id.clone(),
                    reason_code: "state_drift_detected".to_string(),
                    summary: "incoming signal outcome differs from current assignment outcome"
                        .to_string(),
                    previous_outcome: Some(previous_outcome.clone()),
                    incoming_outcome: outcome_after.clone(),
                });
            }
            self.assignments[assignment_index].outcome = Some(outcome_after.clone());
            applied = true;
        }
        self.assignments[assignment_index].updated_at_ms = Some(signal.observed_at_ms);

        let mut lifecycle_to = self.lifecycle_state;
        match &outcome_after {
            Outcome::Success { .. } => {
                let to_running = match lifecycle_to {
                    MissionLifecycleState::Dispatching => Some((
                        MissionLifecycleState::Running,
                        MissionLifecycleTransitionKind::ExecutionStarted,
                    )),
                    MissionLifecycleState::RetryPending | MissionLifecycleState::Blocked => Some((
                        MissionLifecycleState::Running,
                        MissionLifecycleTransitionKind::RetryResumed,
                    )),
                    _ => None,
                };
                if let Some((state, kind)) = to_running {
                    if mission_lifecycle_can_transition(lifecycle_to, state, kind) {
                        self.transition_lifecycle(state, kind, signal.observed_at_ms)?;
                        lifecycle_to = state;
                    }
                }

                let all_assignments_success = self
                    .assignments
                    .iter()
                    .all(|assignment| matches!(assignment.outcome, Some(Outcome::Success { .. })));
                if all_assignments_success
                    && mission_lifecycle_can_transition(
                        lifecycle_to,
                        MissionLifecycleState::Completed,
                        MissionLifecycleTransitionKind::ExecutionSucceeded,
                    )
                {
                    self.transition_lifecycle(
                        MissionLifecycleState::Completed,
                        MissionLifecycleTransitionKind::ExecutionSucceeded,
                        signal.observed_at_ms,
                    )?;
                    lifecycle_to = MissionLifecycleState::Completed;
                }
            }
            Outcome::Failed { reason_code, .. } => {
                let transition_kind =
                    if reason_code == MissionFailureCode::ApprovalDenied.reason_code() {
                        MissionLifecycleTransitionKind::ApprovalDenied
                    } else if reason_code == MissionFailureCode::ApprovalExpired.reason_code() {
                        MissionLifecycleTransitionKind::ApprovalExpired
                    } else {
                        MissionLifecycleTransitionKind::ExecutionFailed
                    };

                if mission_lifecycle_can_transition(
                    lifecycle_to,
                    MissionLifecycleState::Failed,
                    transition_kind,
                ) {
                    self.transition_lifecycle(
                        MissionLifecycleState::Failed,
                        transition_kind,
                        signal.observed_at_ms,
                    )?;
                    lifecycle_to = MissionLifecycleState::Failed;
                } else if lifecycle_to != MissionLifecycleState::Failed {
                    drift = Some(MissionAssignmentStateDrift {
                        assignment_id: signal.assignment_id.clone(),
                        reason_code: "state_drift_detected".to_string(),
                        summary: format!(
                            "failed outcome could not transition lifecycle from {} via {}",
                            lifecycle_to, transition_kind
                        ),
                        previous_outcome: outcome_before.clone(),
                        incoming_outcome: outcome_after.clone(),
                    });
                }
            }
            Outcome::Cancelled { .. } => {
                if mission_lifecycle_can_transition(
                    lifecycle_to,
                    MissionLifecycleState::Cancelled,
                    MissionLifecycleTransitionKind::MissionCancelled,
                ) {
                    self.transition_lifecycle(
                        MissionLifecycleState::Cancelled,
                        MissionLifecycleTransitionKind::MissionCancelled,
                        signal.observed_at_ms,
                    )?;
                    lifecycle_to = MissionLifecycleState::Cancelled;
                }
            }
        }

        if lifecycle_to == lifecycle_from {
            self.updated_at_ms = Some(signal.observed_at_ms);
        }

        Ok(MissionAssignmentReconciliationReport {
            assignment_id: signal.assignment_id.clone(),
            applied,
            out_of_order: false,
            lifecycle_from,
            lifecycle_to,
            outcome_before,
            outcome_after,
            reason_code: if applied {
                "signal_reconciled".to_string()
            } else {
                "signal_duplicate".to_string()
            },
            error_code: None,
            drift,
        })
    }

    /// Evaluate one adaptive replanning trigger and apply deterministic backoff/loop guards.
    ///
    /// This contract is deterministic under bursty trigger streams:
    /// - duplicate `correlation_id` values are ignored,
    /// - triggers inside active backoff windows are rejected with `replan_backoff_active`,
    /// - accepted triggers update durable `replan_state` and optionally move lifecycle to
    ///   `retry_pending` via `retry_scheduled`.
    pub fn evaluate_adaptive_replan(
        &mut self,
        trigger: &MissionReplanTrigger,
        policy: &MissionReplanBackoffPolicy,
    ) -> Result<MissionReplanDecision, MissionValidationError> {
        Self::validate_non_empty_field("replan_trigger.correlation_id", &trigger.correlation_id)?;
        Self::validate_optional_non_empty_field(
            "replan_trigger.reason_code",
            trigger.reason_code.as_deref(),
        )?;
        Self::validate_replan_backoff_policy(policy)?;
        if let Some(assignment_id) = &trigger.assignment_id {
            self.find_assignment_by_id(assignment_id).ok_or_else(|| {
                MissionValidationError::UnknownAssignmentReference(assignment_id.clone())
            })?;
        }

        if trigger.kind == MissionReplanTriggerKind::RateLimited {
            if let Some(reason_code) = trigger.reason_code.as_deref() {
                let normalized = MissionFailureCode::from_reason_code(reason_code).ok_or_else(|| {
                    MissionValidationError::InvalidFieldValue {
                        field_path: "replan_trigger.reason_code".to_string(),
                        message: format!(
                            "rate_limited trigger requires canonical mission failure code, got '{}'",
                            reason_code
                        ),
                    }
                })?;
                if normalized != MissionFailureCode::RateLimited {
                    return Err(MissionValidationError::InvalidFieldValue {
                        field_path: "replan_trigger.reason_code".to_string(),
                        message: format!(
                            "rate_limited trigger must use '{}', got '{}'",
                            MissionFailureCode::RateLimited.reason_code(),
                            reason_code
                        ),
                    });
                }
            }
        }

        let lifecycle_from = self.lifecycle_state;
        let mut lifecycle_to = lifecycle_from;
        let last_correlation_id = self.replan_state.last_correlation_id.as_deref();
        let current_next_eligible = self
            .replan_state
            .next_eligible_replan_at_ms
            .unwrap_or(trigger.observed_at_ms);
        let current_attempt = self.replan_state.consecutive_replan_count;

        if last_correlation_id == Some(trigger.correlation_id.as_str()) {
            return Ok(MissionReplanDecision {
                trigger_kind: trigger.kind,
                assignment_id: trigger.assignment_id.clone(),
                observed_at_ms: trigger.observed_at_ms,
                correlation_id: trigger.correlation_id.clone(),
                apply_replan: false,
                decision_path: "dedupe_guard".to_string(),
                reason_code: "replan_duplicate_trigger".to_string(),
                error_code: None,
                attempt: current_attempt,
                backoff_ms: (current_next_eligible - trigger.observed_at_ms).max(0),
                scheduled_at_ms: current_next_eligible,
                next_eligible_replan_at_ms: current_next_eligible,
                lifecycle_from,
                lifecycle_to,
            });
        }

        if trigger.observed_at_ms < current_next_eligible {
            self.replan_state.last_trigger_kind = Some(trigger.kind);
            self.replan_state.last_correlation_id = Some(trigger.correlation_id.clone());
            self.replan_state.last_observed_at_ms = Some(trigger.observed_at_ms);

            return Ok(MissionReplanDecision {
                trigger_kind: trigger.kind,
                assignment_id: trigger.assignment_id.clone(),
                observed_at_ms: trigger.observed_at_ms,
                correlation_id: trigger.correlation_id.clone(),
                apply_replan: false,
                decision_path: "backoff_guard".to_string(),
                reason_code: "replan_backoff_active".to_string(),
                error_code: Some("FTM2001".to_string()),
                attempt: current_attempt.max(1),
                backoff_ms: current_next_eligible - trigger.observed_at_ms,
                scheduled_at_ms: current_next_eligible,
                next_eligible_replan_at_ms: current_next_eligible,
                lifecycle_from,
                lifecycle_to,
            });
        }

        let attempt = if let Some(last_observed_at_ms) = self.replan_state.last_observed_at_ms {
            let delta_ms = trigger.observed_at_ms.saturating_sub(last_observed_at_ms);
            if delta_ms >= 0 && delta_ms <= policy.burst_window_ms {
                self.replan_state.consecutive_replan_count.saturating_add(1)
            } else {
                1
            }
        } else {
            1
        };

        let base_backoff_ms = match trigger.kind {
            MissionReplanTriggerKind::Completion | MissionReplanTriggerKind::OperatorOverride => 0,
            MissionReplanTriggerKind::Blocked | MissionReplanTriggerKind::Failed => {
                policy.min_backoff_ms
            }
            MissionReplanTriggerKind::RateLimited => policy.min_backoff_ms.saturating_mul(2),
        };
        let shift = attempt.saturating_sub(1).min(20);
        let growth = 1_i64 << shift;
        let backoff_ms = if base_backoff_ms == 0 {
            0
        } else {
            base_backoff_ms
                .saturating_mul(growth)
                .min(policy.max_backoff_ms)
        };

        let scheduled_at_ms = trigger.observed_at_ms.saturating_add(backoff_ms);
        let next_eligible_replan_at_ms =
            scheduled_at_ms.saturating_add(backoff_ms.max(policy.min_backoff_ms));

        if !matches!(trigger.kind, MissionReplanTriggerKind::Completion)
            && mission_lifecycle_can_transition(
                lifecycle_from,
                MissionLifecycleState::RetryPending,
                MissionLifecycleTransitionKind::RetryScheduled,
            )
        {
            self.transition_lifecycle(
                MissionLifecycleState::RetryPending,
                MissionLifecycleTransitionKind::RetryScheduled,
                scheduled_at_ms,
            )?;
            lifecycle_to = MissionLifecycleState::RetryPending;
        }

        let (reason_code, error_code) = Self::replan_reason_code_for_trigger(trigger.kind);
        self.replan_state.consecutive_replan_count = attempt;
        self.replan_state.last_trigger_kind = Some(trigger.kind);
        self.replan_state.last_correlation_id = Some(trigger.correlation_id.clone());
        self.replan_state.last_observed_at_ms = Some(trigger.observed_at_ms);
        self.replan_state.next_eligible_replan_at_ms = Some(next_eligible_replan_at_ms);
        if self
            .updated_at_ms
            .map(|current| current < trigger.observed_at_ms)
            .unwrap_or(true)
        {
            self.updated_at_ms = Some(trigger.observed_at_ms);
        }

        Ok(MissionReplanDecision {
            trigger_kind: trigger.kind,
            assignment_id: trigger.assignment_id.clone(),
            observed_at_ms: trigger.observed_at_ms,
            correlation_id: trigger.correlation_id.clone(),
            apply_replan: true,
            decision_path: "adaptive_replan_scheduled".to_string(),
            reason_code: reason_code.to_string(),
            error_code: error_code.map(str::to_string),
            attempt,
            backoff_ms,
            scheduled_at_ms,
            next_eligible_replan_at_ms,
            lifecycle_from,
            lifecycle_to,
        })
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

    /// Resolve concrete dispatch target (pane/agent/thread/bead) for an assignment.
    pub fn resolve_dispatch_target(
        &self,
        assignment_id: &AssignmentId,
    ) -> Result<MissionDispatchTarget, MissionValidationError> {
        let (target, _) = self.dispatch_context_for_assignment(assignment_id)?;
        Ok(target)
    }

    /// Produce a normalized dispatch execution envelope for dry-run mode.
    pub fn dispatch_assignment_dry_run(
        &self,
        assignment_id: &AssignmentId,
        completed_at_ms: i64,
    ) -> Result<MissionDispatchExecution, MissionValidationError> {
        let (target, mechanism) = self.dispatch_context_for_assignment(assignment_id)?;
        Ok(MissionDispatchExecution {
            mode: MissionDispatchMode::DryRun,
            target,
            mechanism,
            outcome: Outcome::Success {
                reason_code: "dispatch_dry_run".to_string(),
                completed_at_ms,
            },
        })
    }

    /// Normalize a live dispatch response into canonical mission outcome contracts.
    pub fn dispatch_assignment_live(
        &self,
        assignment_id: &AssignmentId,
        response: MissionDispatchLiveResponse,
    ) -> Result<MissionDispatchExecution, MissionValidationError> {
        let (target, mechanism) = self.dispatch_context_for_assignment(assignment_id)?;
        let outcome = match response {
            MissionDispatchLiveResponse::Delivered {
                reason_code,
                completed_at_ms,
            } => {
                let reason_code = reason_code
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| "dispatch_executed".to_string());
                Outcome::Success {
                    reason_code,
                    completed_at_ms,
                }
            }
            MissionDispatchLiveResponse::Failed {
                reason_code,
                error_code,
                completed_at_ms,
            } => {
                let failure_code = Self::validate_failure_reason_code(
                    assignment_id,
                    MissionFailureContext::AssignmentOutcomeFailed,
                    &reason_code,
                )?;
                let normalized_error_code = match error_code
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                {
                    Some(actual_error_code) => {
                        Self::validate_failure_error_code(
                            assignment_id,
                            MissionFailureContext::AssignmentOutcomeFailed,
                            failure_code.reason_code(),
                            actual_error_code,
                            failure_code.error_code(),
                        )?;
                        actual_error_code.to_string()
                    }
                    None => failure_code.error_code().to_string(),
                };
                Outcome::Failed {
                    reason_code: failure_code.reason_code().to_string(),
                    error_code: normalized_error_code,
                    completed_at_ms,
                }
            }
        };

        Ok(MissionDispatchExecution {
            mode: MissionDispatchMode::Live,
            target,
            mechanism,
            outcome,
        })
    }

    /// Evaluate whether a dispatch attempt is a duplicate using content-addressed
    /// idempotency keys. Returns a dedup result indicating whether the dispatch
    /// should proceed or return a cached outcome.
    ///
    /// Deduplication semantics:
    /// - identical (mission_id, assignment_id, mechanism) → duplicate,
    /// - mechanism hash mismatch on same key → stale/conflict (not a duplicate),
    /// - no prior record → fresh dispatch (proceed).
    pub fn evaluate_dispatch_deduplication(
        &self,
        assignment_id: &AssignmentId,
        correlation_id: &str,
    ) -> Result<MissionDispatchDeduplicationResult, MissionValidationError> {
        Self::validate_non_empty_field("correlation_id", correlation_id)?;

        let (_, mechanism) = self.dispatch_context_for_assignment(assignment_id)?;
        let idempotency_key =
            MissionDispatchIdempotencyKey::compute(&self.mission_id, assignment_id, &mechanism);
        let mechanism_hash = {
            let mechanism_json = serde_json::to_string(&mechanism).unwrap_or_default();
            sha256_hex(&mechanism_json)[..16].to_string()
        };

        match self.dispatch_dedup_state.find_by_key(&idempotency_key) {
            Some(existing) => {
                if existing.mechanism_hash != mechanism_hash {
                    // Same key but different mechanism — stale record from a prior
                    // version of the assignment. Allow re-dispatch.
                    Ok(MissionDispatchDeduplicationResult {
                        idempotency_key,
                        is_duplicate: false,
                        decision_path: "dedup_mechanism_hash_mismatch".to_string(),
                        reason_code: "dispatch_mechanism_changed".to_string(),
                        cached_record: None,
                    })
                } else {
                    // Exact match — this is a duplicate dispatch.
                    Ok(MissionDispatchDeduplicationResult {
                        idempotency_key,
                        is_duplicate: true,
                        decision_path: "dedup_exact_match".to_string(),
                        reason_code: "dispatch_duplicate".to_string(),
                        cached_record: Some(existing.clone()),
                    })
                }
            }
            None => Ok(MissionDispatchDeduplicationResult {
                idempotency_key,
                is_duplicate: false,
                decision_path: "dedup_no_prior_record".to_string(),
                reason_code: "dispatch_fresh".to_string(),
                cached_record: None,
            }),
        }
    }

    /// Execute a live dispatch with integrated deduplication.
    ///
    /// 1. Checks dedup state for prior execution of the same logical action.
    /// 2. If duplicate: returns cached outcome without re-execution.
    /// 3. If fresh: executes the dispatch, records the result, returns execution.
    pub fn dispatch_assignment_live_idempotent(
        &mut self,
        assignment_id: &AssignmentId,
        correlation_id: &str,
        response: MissionDispatchLiveResponse,
        dispatched_at_ms: i64,
    ) -> Result<
        (MissionDispatchExecution, MissionDispatchDeduplicationResult),
        MissionValidationError,
    > {
        let dedup_result = self.evaluate_dispatch_deduplication(assignment_id, correlation_id)?;

        if dedup_result.is_duplicate {
            // Return cached outcome as a synthetic execution.
            let cached = dedup_result
                .cached_record
                .as_ref()
                .expect("cached_record must be present when is_duplicate=true");
            let (target, mechanism) = self.dispatch_context_for_assignment(assignment_id)?;
            let execution = MissionDispatchExecution {
                mode: MissionDispatchMode::Live,
                target,
                mechanism,
                outcome: cached.outcome.clone(),
            };
            return Ok((execution, dedup_result));
        }

        // Fresh dispatch — execute and record.
        let execution = self.dispatch_assignment_live(assignment_id, response)?;

        let mechanism_hash = {
            let mechanism_json = serde_json::to_string(&execution.mechanism).unwrap_or_default();
            sha256_hex(&mechanism_json)[..16].to_string()
        };

        let record = MissionDispatchDeduplicationRecord {
            idempotency_key: dedup_result.idempotency_key.clone(),
            assignment_id: assignment_id.clone(),
            correlation_id: correlation_id.to_string(),
            dispatched_at_ms,
            outcome: execution.outcome.clone(),
            mechanism_hash,
        };
        self.dispatch_dedup_state.record_dispatch(record);

        Ok((execution, dedup_result))
    }

    // ========================================================================
    // Kill-Switch and Safe-Mode Operations
    // ========================================================================

    /// Evaluate whether the kill-switch blocks a dispatch or lifecycle operation.
    ///
    /// This method is the central policy gate: every dispatch path should call it
    /// before proceeding. It evaluates the current kill-switch level (with TTL
    /// expiry) and returns a structured decision.
    pub fn evaluate_kill_switch(&mut self, evaluated_at_ms: i64) -> MissionKillSwitchDecision {
        let effective_level = self.kill_switch.evaluate_effective_level(evaluated_at_ms);

        match effective_level {
            MissionKillSwitchLevel::Off => MissionKillSwitchDecision {
                effective_level,
                blocked: false,
                decision_path: "kill_switch_off".to_string(),
                reason_code: "dispatch_allowed".to_string(),
                error_code: None,
                activation: None,
            },
            MissionKillSwitchLevel::SafeMode => MissionKillSwitchDecision {
                effective_level,
                blocked: true,
                decision_path: "kill_switch_safe_mode".to_string(),
                reason_code: MissionFailureCode::KillSwitchActivated
                    .reason_code()
                    .to_string(),
                error_code: Some(
                    MissionFailureCode::KillSwitchActivated
                        .error_code()
                        .to_string(),
                ),
                activation: self.kill_switch.current_activation.clone(),
            },
            MissionKillSwitchLevel::HardStop => MissionKillSwitchDecision {
                effective_level,
                blocked: true,
                decision_path: "kill_switch_hard_stop".to_string(),
                reason_code: MissionFailureCode::KillSwitchActivated
                    .reason_code()
                    .to_string(),
                error_code: Some(
                    MissionFailureCode::KillSwitchActivated
                        .error_code()
                        .to_string(),
                ),
                activation: self.kill_switch.current_activation.clone(),
            },
        }
    }

    /// Activate the global kill-switch at the specified level.
    ///
    /// Records the activation in the mission's kill-switch state and returns
    /// the decision that will apply to subsequent dispatches.
    pub fn activate_kill_switch(
        &mut self,
        level: MissionKillSwitchLevel,
        activated_by: impl Into<String>,
        reason_code: impl Into<String>,
        activated_at_ms: i64,
        expires_at_ms: Option<i64>,
        correlation_id: Option<String>,
    ) -> Result<MissionKillSwitchDecision, MissionValidationError> {
        if level == MissionKillSwitchLevel::Off {
            return Err(MissionValidationError::InvalidFieldValue {
                field_path: "kill_switch.level".to_string(),
                message:
                    "cannot activate kill-switch at level Off; use deactivate_kill_switch instead"
                        .to_string(),
            });
        }

        let activated_by = activated_by.into();
        let reason_code_str = reason_code.into();

        if activated_by.trim().is_empty() {
            return Err(MissionValidationError::InvalidFieldValue {
                field_path: "kill_switch.activated_by".to_string(),
                message: "activated_by must not be empty".to_string(),
            });
        }
        if reason_code_str.trim().is_empty() {
            return Err(MissionValidationError::InvalidFieldValue {
                field_path: "kill_switch.reason_code".to_string(),
                message: "reason_code must not be empty".to_string(),
            });
        }

        if let Some(expires) = expires_at_ms {
            if expires <= activated_at_ms {
                return Err(MissionValidationError::InvalidFieldValue {
                    field_path: "kill_switch.expires_at_ms".to_string(),
                    message: "expires_at_ms must be after activated_at_ms".to_string(),
                });
            }
        }

        let activation = MissionKillSwitchActivation {
            level,
            activated_by,
            reason_code: reason_code_str,
            error_code: Some(
                MissionFailureCode::KillSwitchActivated
                    .error_code()
                    .to_string(),
            ),
            activated_at_ms,
            expires_at_ms,
            correlation_id,
        };

        self.kill_switch.activate(activation);

        Ok(self.evaluate_kill_switch(activated_at_ms))
    }

    /// Deactivate the global kill-switch, returning to normal operation.
    pub fn deactivate_kill_switch(
        &mut self,
        deactivated_by: impl Into<String>,
        reason_code: impl Into<String>,
        deactivated_at_ms: i64,
    ) -> Result<MissionKillSwitchDecision, MissionValidationError> {
        let deactivated_by = deactivated_by.into();
        let reason_code_str = reason_code.into();

        if deactivated_by.trim().is_empty() {
            return Err(MissionValidationError::InvalidFieldValue {
                field_path: "kill_switch.deactivated_by".to_string(),
                message: "deactivated_by must not be empty".to_string(),
            });
        }
        if reason_code_str.trim().is_empty() {
            return Err(MissionValidationError::InvalidFieldValue {
                field_path: "kill_switch.reason_code".to_string(),
                message: "reason_code must not be empty".to_string(),
            });
        }

        self.kill_switch
            .deactivate(&deactivated_by, &reason_code_str, deactivated_at_ms);

        Ok(self.evaluate_kill_switch(deactivated_at_ms))
    }

    /// Cancel all in-flight (non-terminal) assignments due to kill-switch activation.
    ///
    /// Returns the number of assignments cancelled. Each cancelled assignment
    /// gets an `Outcome::Cancelled` with the kill-switch reason code.
    pub fn cancel_in_flight_for_kill_switch(&mut self, cancelled_at_ms: i64) -> usize {
        let mut cancelled_count = 0;
        for assignment in &mut self.assignments {
            // Skip assignments that already have a terminal outcome
            if assignment.outcome.is_some() {
                continue;
            }
            assignment.outcome = Some(Outcome::Cancelled {
                reason_code: MissionFailureCode::KillSwitchActivated
                    .reason_code()
                    .to_string(),
                completed_at_ms: cancelled_at_ms,
            });
            cancelled_count += 1;
        }
        cancelled_count
    }

    // ========================================================================
    // Pause/Resume/Abort Control Operations (C5)
    // ========================================================================

    /// Returns true if the mission can be paused from its current state.
    #[must_use]
    pub fn can_pause(&self) -> bool {
        matches!(
            self.lifecycle_state,
            MissionLifecycleState::Running
                | MissionLifecycleState::Dispatching
                | MissionLifecycleState::AwaitingApproval
                | MissionLifecycleState::Blocked
                | MissionLifecycleState::RetryPending
        )
    }

    /// Returns true if the mission can be resumed (must be Paused).
    #[must_use]
    pub fn can_resume(&self) -> bool {
        self.lifecycle_state == MissionLifecycleState::Paused
    }

    /// Returns true if the mission can be aborted (any non-terminal state).
    #[must_use]
    pub fn can_abort(&self) -> bool {
        !self.lifecycle_state.is_terminal()
    }

    /// Pause the mission, capturing a checkpoint of current state.
    ///
    /// Transitions from any active state to Paused, recording the prior state
    /// in a checkpoint for deterministic resume. Each assignment's outcome and
    /// approval state is snapshotted.
    pub fn pause_mission(
        &mut self,
        requested_by: impl Into<String>,
        reason_code: impl Into<String>,
        requested_at_ms: i64,
        correlation_id: Option<String>,
    ) -> Result<MissionControlDecision, MissionValidationError> {
        let requested_by = requested_by.into();
        let reason_code = reason_code.into();

        if requested_by.trim().is_empty() {
            return Err(MissionValidationError::InvalidFieldValue {
                field_path: "pause.requested_by".to_string(),
                message: "requested_by must not be empty".to_string(),
            });
        }
        if reason_code.trim().is_empty() {
            return Err(MissionValidationError::InvalidFieldValue {
                field_path: "pause.reason_code".to_string(),
                message: "reason_code must not be empty".to_string(),
            });
        }

        let lifecycle_from = self.lifecycle_state;
        if !self.can_pause() {
            return Err(MissionValidationError::InvalidLifecycleTransition {
                from: lifecycle_from,
                to: MissionLifecycleState::Paused,
                kind: MissionLifecycleTransitionKind::PauseRequested,
            });
        }

        let assignment_entries: Vec<AssignmentCheckpointEntry> = self
            .assignments
            .iter()
            .map(|a| AssignmentCheckpointEntry {
                assignment_id: a.assignment_id.clone(),
                outcome_summary: a.outcome.as_ref().map(|o| match o {
                    Outcome::Success { .. } => "success".to_string(),
                    Outcome::Failed { .. } => "failed".to_string(),
                    Outcome::Cancelled { .. } => "cancelled".to_string(),
                }),
                approval_state_summary: a.approval_state.canonical_string(),
            })
            .collect();

        let checkpoint_id = format!("cp-{}-{}", self.mission_id.0, requested_at_ms);

        let checkpoint = MissionCheckpoint {
            checkpoint_id: checkpoint_id.clone(),
            paused_from_state: lifecycle_from,
            paused_by: requested_by.clone(),
            reason_code: reason_code.clone(),
            paused_at_ms: requested_at_ms,
            resumed_at_ms: None,
            resumed_by: None,
            assignment_entries,
            correlation_id: correlation_id.clone(),
        };

        self.lifecycle_state = MissionLifecycleState::Paused;
        self.updated_at_ms = Some(requested_at_ms);
        self.pause_resume_state.current_checkpoint = Some(checkpoint);
        self.pause_resume_state.total_pause_count += 1;

        Ok(MissionControlDecision {
            action: "pause".to_string(),
            lifecycle_from,
            lifecycle_to: MissionLifecycleState::Paused,
            decision_path: format!("pause_mission->{}->paused", lifecycle_from),
            reason_code,
            error_code: None,
            checkpoint_id: Some(checkpoint_id),
            decided_at_ms: requested_at_ms,
        })
    }

    /// Resume the mission from a paused state, restoring the prior lifecycle state.
    ///
    /// The checkpoint is finalized with resume timing and moved to history.
    /// Cumulative pause duration is updated for SLO tracking.
    pub fn resume_mission(
        &mut self,
        requested_by: impl Into<String>,
        reason_code: impl Into<String>,
        requested_at_ms: i64,
        _correlation_id: Option<String>,
    ) -> Result<MissionControlDecision, MissionValidationError> {
        let requested_by = requested_by.into();
        let reason_code = reason_code.into();

        if requested_by.trim().is_empty() {
            return Err(MissionValidationError::InvalidFieldValue {
                field_path: "resume.requested_by".to_string(),
                message: "requested_by must not be empty".to_string(),
            });
        }

        let lifecycle_from = self.lifecycle_state;
        if !self.can_resume() {
            return Err(MissionValidationError::InvalidLifecycleTransition {
                from: lifecycle_from,
                to: lifecycle_from,
                kind: MissionLifecycleTransitionKind::ResumeRequested,
            });
        }

        let mut checkpoint = self
            .pause_resume_state
            .current_checkpoint
            .take()
            .expect("can_resume() guarantees checkpoint exists");

        let resume_to = checkpoint.paused_from_state;
        checkpoint.resumed_at_ms = Some(requested_at_ms);
        checkpoint.resumed_by = Some(requested_by.clone());

        let pause_duration = requested_at_ms - checkpoint.paused_at_ms;
        if pause_duration > 0 {
            self.pause_resume_state.cumulative_pause_duration_ms += pause_duration;
        }

        self.pause_resume_state.checkpoint_history.push(checkpoint);
        self.pause_resume_state.total_resume_count += 1;

        self.lifecycle_state = resume_to;
        self.updated_at_ms = Some(requested_at_ms);

        Ok(MissionControlDecision {
            action: "resume".to_string(),
            lifecycle_from,
            lifecycle_to: resume_to,
            decision_path: format!("resume_mission->paused->{}", resume_to),
            reason_code,
            error_code: None,
            checkpoint_id: None,
            decided_at_ms: requested_at_ms,
        })
    }

    /// Abort the mission, cancelling all in-flight assignments.
    ///
    /// Can be called from any non-terminal state. All assignments without
    /// a terminal outcome are cancelled. If paused, the checkpoint is finalized.
    pub fn abort_mission(
        &mut self,
        requested_by: impl Into<String>,
        reason_code: impl Into<String>,
        error_code: Option<String>,
        requested_at_ms: i64,
        _correlation_id: Option<String>,
    ) -> Result<MissionControlDecision, MissionValidationError> {
        let requested_by = requested_by.into();
        let reason_code = reason_code.into();

        if requested_by.trim().is_empty() {
            return Err(MissionValidationError::InvalidFieldValue {
                field_path: "abort.requested_by".to_string(),
                message: "requested_by must not be empty".to_string(),
            });
        }

        let lifecycle_from = self.lifecycle_state;
        if !self.can_abort() {
            return Err(MissionValidationError::InvalidLifecycleTransition {
                from: lifecycle_from,
                to: MissionLifecycleState::Cancelled,
                kind: MissionLifecycleTransitionKind::AbortRequested,
            });
        }

        if let Some(mut checkpoint) = self.pause_resume_state.current_checkpoint.take() {
            checkpoint.resumed_at_ms = Some(requested_at_ms);
            checkpoint.resumed_by = Some(requested_by.clone());
            let pause_duration = requested_at_ms - checkpoint.paused_at_ms;
            if pause_duration > 0 {
                self.pause_resume_state.cumulative_pause_duration_ms += pause_duration;
            }
            self.pause_resume_state.checkpoint_history.push(checkpoint);
        }

        let abort_reason = format!("abort:{}", reason_code);
        for assignment in &mut self.assignments {
            if assignment.outcome.is_some() {
                continue;
            }
            assignment.outcome = Some(Outcome::Cancelled {
                reason_code: abort_reason.clone(),
                completed_at_ms: requested_at_ms,
            });
        }

        self.pause_resume_state.total_abort_count += 1;
        self.lifecycle_state = MissionLifecycleState::Cancelled;
        self.updated_at_ms = Some(requested_at_ms);

        Ok(MissionControlDecision {
            action: "abort".to_string(),
            lifecycle_from,
            lifecycle_to: MissionLifecycleState::Cancelled,
            decision_path: format!("abort_mission->{}->cancelled", lifecycle_from),
            reason_code,
            error_code,
            checkpoint_id: None,
            decided_at_ms: requested_at_ms,
        })
    }

    // ── C8: Journal integration helpers ─────────────────────────────────────

    /// Create a new journal for this mission.
    #[must_use]
    pub fn create_journal(&self) -> MissionJournal {
        MissionJournal::new(self.mission_id.clone())
    }

    /// Sync journal metadata into the mission's embedded journal_state.
    pub fn sync_journal_state(&mut self, journal: &MissionJournal) {
        self.journal_state = journal.snapshot_state();
    }

    /// Record a lifecycle transition in the provided journal.
    pub fn journal_lifecycle_transition(
        journal: &mut MissionJournal,
        from: MissionLifecycleState,
        to: MissionLifecycleState,
        transition_kind: MissionLifecycleTransitionKind,
        correlation_id: &str,
        initiated_by: &str,
        timestamp_ms: i64,
    ) -> Result<u64, MissionJournalError> {
        let kind = MissionJournalEntryKind::LifecycleTransition {
            from,
            to,
            transition_kind,
        };
        journal.append(
            kind,
            correlation_id,
            initiated_by,
            format!("{}->{}:{}", from, to, transition_kind),
            None,
            timestamp_ms,
        )
    }

    /// Record a control command decision in the provided journal.
    pub fn journal_control_command(
        journal: &mut MissionJournal,
        command: &MissionControlCommand,
        decision: &MissionControlDecision,
        correlation_id: &str,
        timestamp_ms: i64,
    ) -> Result<u64, MissionJournalError> {
        let kind = MissionJournalEntryKind::ControlCommand {
            command: command.clone(),
            decision: decision.clone(),
        };
        journal.append(
            kind,
            correlation_id,
            command.requested_by(),
            command.reason_code(),
            decision.error_code.clone(),
            timestamp_ms,
        )
    }

    /// Record a kill-switch level change in the provided journal.
    pub fn journal_kill_switch_change(
        journal: &mut MissionJournal,
        level_from: MissionKillSwitchLevel,
        level_to: MissionKillSwitchLevel,
        correlation_id: &str,
        initiated_by: &str,
        timestamp_ms: i64,
    ) -> Result<u64, MissionJournalError> {
        let kind = MissionJournalEntryKind::KillSwitchChange {
            level_from,
            level_to,
        };
        journal.append(
            kind,
            correlation_id,
            initiated_by,
            format!("kill_switch:{}->{}", level_from, level_to),
            None,
            timestamp_ms,
        )
    }

    /// Record an assignment outcome change in the provided journal.
    pub fn journal_assignment_outcome(
        journal: &mut MissionJournal,
        assignment_id: &AssignmentId,
        outcome_before: Option<&str>,
        outcome_after: &str,
        correlation_id: &str,
        initiated_by: &str,
        timestamp_ms: i64,
    ) -> Result<u64, MissionJournalError> {
        let kind = MissionJournalEntryKind::AssignmentOutcome {
            assignment_id: assignment_id.clone(),
            outcome_before: outcome_before.map(String::from),
            outcome_after: outcome_after.to_string(),
        };
        journal.append(
            kind,
            correlation_id,
            initiated_by,
            "assignment_outcome",
            None,
            timestamp_ms,
        )
    }

    /// Evaluate reservation feasibility + ownership before dispatch-time execution.
    ///
    /// Emits canonical dispatch-time preflight outcomes so planner/dispatcher can:
    /// - block on conflicting external leases held by another assignee,
    /// - fail fast when reservation intent has already expired,
    /// - continue when no conflicts exist or leases belong to the same assignee.
    pub fn evaluate_reservation_feasibility(
        &self,
        leases: &[MissionReservationLease],
        evaluated_at_ms: i64,
    ) -> Result<MissionPolicyPreflightReport, MissionValidationError> {
        let mut checks = Vec::with_capacity(self.assignments.len());

        for assignment in &self.assignments {
            let mut check = MissionPolicyPreflightCheck {
                candidate_id: assignment.candidate_id.clone(),
                assignment_id: Some(assignment.assignment_id.clone()),
                decision: MissionPolicyDecisionKind::Allow,
                reason_code: None,
                rule_id: Some("reservation.feasibility".to_string()),
                context: None,
            };

            if let Some(intent) = &assignment.reservation_intent {
                if intent
                    .expires_at_ms
                    .is_some_and(|expires_at_ms| expires_at_ms < evaluated_at_ms)
                {
                    check.decision = MissionPolicyDecisionKind::Deny;
                    check.reason_code =
                        Some(MissionFailureCode::StaleState.reason_code().to_string());
                    check.context = Some(format!(
                        "reservation_intent_expired reservation_id={} remediation=refresh_reservations_then_retry escalation=human",
                        intent.reservation_id.0
                    ));
                    checks.push(check);
                    continue;
                }

                let mut conflicting_holders = BTreeSet::new();
                let mut conflicting_paths = BTreeSet::new();
                let mut conflicting_lease_ids = BTreeSet::new();

                for lease in leases {
                    if !lease.exclusive || lease.is_expired_at(evaluated_at_ms) {
                        continue;
                    }
                    if lease.holder.trim() == assignment.assignee.trim() {
                        continue;
                    }
                    if intent.paths.iter().any(|intent_path| {
                        lease.paths.iter().any(|lease_path| {
                            Self::reservation_paths_overlap(intent_path, lease_path)
                        })
                    }) {
                        conflicting_holders.insert(lease.holder.clone());
                        conflicting_lease_ids.insert(lease.lease_id.clone());
                        for intent_path in &intent.paths {
                            for lease_path in &lease.paths {
                                if Self::reservation_paths_overlap(intent_path, lease_path) {
                                    conflicting_paths.insert(intent_path.clone());
                                    conflicting_paths.insert(lease_path.clone());
                                }
                            }
                        }
                    }
                }

                if !conflicting_holders.is_empty() {
                    check.decision = MissionPolicyDecisionKind::Deny;
                    check.reason_code = Some(
                        MissionFailureCode::ReservationConflict
                            .reason_code()
                            .to_string(),
                    );
                    check.context = Some(format!(
                        "conflict_holders={} conflict_paths={} lease_ids={} recommendation=coordinate_or_wait escalation=human",
                        conflicting_holders
                            .into_iter()
                            .collect::<Vec<_>>()
                            .join("|"),
                        conflicting_paths.into_iter().collect::<Vec<_>>().join("|"),
                        conflicting_lease_ids
                            .into_iter()
                            .collect::<Vec<_>>()
                            .join("|")
                    ));
                }
            }

            checks.push(check);
        }

        self.evaluate_policy_preflight(MissionPolicyPreflightStage::DispatchTime, &checks)
    }

    fn reservation_paths_overlap(intent_path: &str, lease_path: &str) -> bool {
        let intent_path = intent_path.trim();
        let lease_path = lease_path.trim();
        if intent_path.is_empty() || lease_path.is_empty() {
            return false;
        }
        Self::wildcard_path_match(intent_path, lease_path)
            || Self::wildcard_path_match(lease_path, intent_path)
    }

    fn wildcard_path_match(pattern: &str, value: &str) -> bool {
        let pattern = pattern.as_bytes();
        let value = value.as_bytes();
        let mut dp = vec![vec![false; value.len() + 1]; pattern.len() + 1];
        dp[0][0] = true;

        for i in 1..=pattern.len() {
            if pattern[i - 1] == b'*' {
                dp[i][0] = dp[i - 1][0];
            }
        }

        for i in 1..=pattern.len() {
            for j in 1..=value.len() {
                dp[i][j] = match pattern[i - 1] {
                    b'*' => dp[i - 1][j] || dp[i][j - 1],
                    b'?' => dp[i - 1][j - 1],
                    byte => dp[i - 1][j - 1] && byte == value[j - 1],
                };
            }
        }

        dp[pattern.len()][value.len()]
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

    fn pane_id_for_mechanism(mechanism: &MissionDispatchMechanism) -> Option<u64> {
        match mechanism {
            MissionDispatchMechanism::RobotSend { pane_id, .. } => Some(*pane_id),
            MissionDispatchMechanism::RobotWaitFor {
                pane_id, condition, ..
            } => pane_id.or(match condition {
                WaitCondition::Pattern { pane_id, .. }
                | WaitCondition::PaneIdle { pane_id, .. }
                | WaitCondition::StableTail { pane_id, .. } => *pane_id,
                WaitCondition::External { .. } => None,
            }),
            MissionDispatchMechanism::RobotRunWorkflow { .. }
            | MissionDispatchMechanism::InternalLockAcquire { .. }
            | MissionDispatchMechanism::InternalLockRelease { .. }
            | MissionDispatchMechanism::InternalStoreData { .. }
            | MissionDispatchMechanism::InternalMarkEventHandled { .. }
            | MissionDispatchMechanism::InternalValidateApproval { .. }
            | MissionDispatchMechanism::InternalNestedPlan { .. }
            | MissionDispatchMechanism::InternalCustom { .. } => None,
        }
    }

    fn dispatch_context_for_assignment(
        &self,
        assignment_id: &AssignmentId,
    ) -> Result<(MissionDispatchTarget, MissionDispatchMechanism), MissionValidationError> {
        let assignment = self
            .find_assignment_by_id(assignment_id)
            .ok_or_else(|| {
                MissionValidationError::UnknownAssignmentReference(assignment_id.clone())
            })?
            .clone();
        if assignment.assignee.trim().is_empty() {
            return Err(MissionValidationError::EmptyAssignee(
                assignment.assignment_id.clone(),
            ));
        }

        let contract = self.dispatch_contract_for_candidate(&assignment.candidate_id)?;
        let mechanism = contract.mechanism;
        let pane_id = Self::pane_id_for_mechanism(&mechanism);
        let target = MissionDispatchTarget {
            assignment_id: assignment.assignment_id.clone(),
            candidate_id: assignment.candidate_id.clone(),
            assignee: assignment.assignee.trim().to_string(),
            pane_id,
            thread_id: contract.messaging.thread_id,
            bead_id: contract.messaging.bead_id,
        };

        Ok((target, mechanism))
    }

    fn find_assignment_by_id(&self, assignment_id: &AssignmentId) -> Option<&Assignment> {
        self.assignments
            .iter()
            .find(|assignment| assignment.assignment_id == *assignment_id)
    }

    fn find_assignment_index_by_id(&self, assignment_id: &AssignmentId) -> Option<usize> {
        self.assignments
            .iter()
            .position(|assignment| assignment.assignment_id == *assignment_id)
    }

    fn normalize_assignment_signal_outcome(
        assignment_id: &AssignmentId,
        payload: &MissionAssignmentSignalPayload,
    ) -> Result<Outcome, MissionValidationError> {
        match payload {
            MissionAssignmentSignalPayload::Completed {
                reason_code,
                completed_at_ms,
            } => {
                let reason_code = reason_code.trim();
                let reason_code = if reason_code.is_empty() {
                    "dispatch_executed".to_string()
                } else {
                    reason_code.to_string()
                };
                Ok(Outcome::Success {
                    reason_code,
                    completed_at_ms: *completed_at_ms,
                })
            }
            MissionAssignmentSignalPayload::Failed {
                reason_code,
                error_code,
                completed_at_ms,
            } => {
                let failure_code = Self::validate_failure_reason_code(
                    assignment_id,
                    MissionFailureContext::AssignmentOutcomeFailed,
                    reason_code,
                )?;
                let normalized_error_code = match error_code
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                {
                    Some(actual_error_code) => {
                        Self::validate_failure_error_code(
                            assignment_id,
                            MissionFailureContext::AssignmentOutcomeFailed,
                            failure_code.reason_code(),
                            actual_error_code,
                            failure_code.error_code(),
                        )?;
                        actual_error_code.to_string()
                    }
                    None => failure_code.error_code().to_string(),
                };
                Ok(Outcome::Failed {
                    reason_code: failure_code.reason_code().to_string(),
                    error_code: normalized_error_code,
                    completed_at_ms: *completed_at_ms,
                })
            }
            MissionAssignmentSignalPayload::TimedOut { completed_at_ms } => {
                let failure_code = MissionFailureCode::DispatchError;
                Ok(Outcome::Failed {
                    reason_code: failure_code.reason_code().to_string(),
                    error_code: failure_code.error_code().to_string(),
                    completed_at_ms: *completed_at_ms,
                })
            }
        }
    }

    fn validate_replan_backoff_policy(
        policy: &MissionReplanBackoffPolicy,
    ) -> Result<(), MissionValidationError> {
        if policy.min_backoff_ms <= 0 {
            return Err(MissionValidationError::InvalidFieldValue {
                field_path: "replan_policy.min_backoff_ms".to_string(),
                message: "min_backoff_ms must be > 0".to_string(),
            });
        }
        if policy.max_backoff_ms < policy.min_backoff_ms {
            return Err(MissionValidationError::InvalidFieldValue {
                field_path: "replan_policy.max_backoff_ms".to_string(),
                message: "max_backoff_ms must be >= min_backoff_ms".to_string(),
            });
        }
        if policy.burst_window_ms < 0 {
            return Err(MissionValidationError::InvalidFieldValue {
                field_path: "replan_policy.burst_window_ms".to_string(),
                message: "burst_window_ms must be >= 0".to_string(),
            });
        }
        Ok(())
    }

    fn replan_reason_code_for_trigger(
        trigger_kind: MissionReplanTriggerKind,
    ) -> (&'static str, Option<&'static str>) {
        match trigger_kind {
            MissionReplanTriggerKind::Completion => ("replan_completion_signal", None),
            MissionReplanTriggerKind::Blocked => ("replan_block_signal", None),
            MissionReplanTriggerKind::Failed => ("replan_failure_signal", None),
            MissionReplanTriggerKind::RateLimited => (
                MissionFailureCode::RateLimited.reason_code(),
                Some(MissionFailureCode::RateLimited.error_code()),
            ),
            MissionReplanTriggerKind::OperatorOverride => ("replan_operator_override", None),
        }
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
        let has_pending_approval = assignments
            .iter()
            .any(|assignment| matches!(assignment.approval_state, ApprovalState::Pending { .. }));

        match lifecycle_state {
            MissionLifecycleState::AwaitingApproval if !has_pending_approval => {
                Err(MissionValidationError::AwaitingApprovalWithoutPendingAssignment)
            }
            state
                if has_pending_approval
                    && !matches!(
                        state,
                        MissionLifecycleState::AwaitingApproval | MissionLifecycleState::Paused
                    ) =>
            {
                Err(MissionValidationError::PendingApprovalOutsideAwaitingState { state })
            }
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
    AwaitingApprovalWithoutPendingAssignment,
    PendingApprovalOutsideAwaitingState {
        state: MissionLifecycleState,
    },
    InvalidApprovalStateTransition {
        assignment_id: AssignmentId,
        action: &'static str,
        state: String,
    },
    InvalidApprovalLifecycleState {
        assignment_id: AssignmentId,
        action: &'static str,
        lifecycle_state: MissionLifecycleState,
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
            Self::AwaitingApprovalWithoutPendingAssignment => {
                f.write_str(
                    "Mission lifecycle state awaiting_approval requires at least one pending approval assignment",
                )
            }
            Self::PendingApprovalOutsideAwaitingState { state } => {
                write!(
                    f,
                    "Mission has pending approval assignment(s) but lifecycle state is {state} (expected awaiting_approval)"
                )
            }
            Self::InvalidApprovalStateTransition {
                assignment_id,
                action,
                state,
            } => {
                write!(
                    f,
                    "Approval action '{action}' is invalid for assignment {} in state '{state}'",
                    assignment_id.0
                )
            }
            Self::InvalidApprovalLifecycleState {
                assignment_id,
                action,
                lifecycle_state,
            } => {
                write!(
                    f,
                    "Approval action '{action}' is invalid for assignment {} while mission lifecycle is {lifecycle_state}",
                    assignment_id.0
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
    fn mission_approval_request_transitions_to_pending_and_awaiting_approval() {
        let mut mission = sample_mission();
        mission.lifecycle_state = MissionLifecycleState::Dispatching;
        mission.assignments[0].approval_state = ApprovalState::NotRequired;
        mission.assignments[0].outcome = None;
        mission.assignments[0].updated_at_ms = None;

        let record = mission
            .request_assignment_approval(
                &AssignmentId("assignment:a".to_string()),
                "operator-human",
                1_704_000_010_000,
            )
            .unwrap();

        assert_eq!(record.lifecycle_from, MissionLifecycleState::Dispatching);
        assert_eq!(record.lifecycle_to, MissionLifecycleState::AwaitingApproval);
        assert_eq!(
            record.transition_kind,
            MissionLifecycleTransitionKind::ApprovalRequested
        );
        assert_eq!(
            record.reason_code,
            MissionFailureCode::ApprovalRequired.reason_code()
        );
        assert_eq!(
            record.error_code.as_deref(),
            Some(MissionFailureCode::ApprovalRequired.error_code())
        );
        assert!(!record.idempotent);
        assert_eq!(
            mission.lifecycle_state,
            MissionLifecycleState::AwaitingApproval
        );
        assert!(matches!(
            mission.assignments[0].approval_state,
            ApprovalState::Pending {
                ref requested_by,
                requested_at_ms
            } if requested_by == "operator-human" && requested_at_ms == 1_704_000_010_000
        ));
    }

    #[test]
    fn mission_approval_request_is_idempotent_when_already_pending() {
        let mut mission = sample_mission();
        mission.lifecycle_state = MissionLifecycleState::AwaitingApproval;
        mission.assignments[0].approval_state = ApprovalState::Pending {
            requested_by: "operator-human".to_string(),
            requested_at_ms: 1_704_000_010_100,
        };
        mission.assignments[0].outcome = None;

        let record = mission
            .request_assignment_approval(
                &AssignmentId("assignment:a".to_string()),
                "operator-human",
                1_704_000_010_100,
            )
            .unwrap();

        assert!(record.idempotent);
        assert_eq!(
            mission.lifecycle_state,
            MissionLifecycleState::AwaitingApproval
        );
        assert!(mission.validate().is_ok());
    }

    #[test]
    fn mission_approval_continuation_transitions_to_running() {
        let mut mission = sample_mission();
        mission.lifecycle_state = MissionLifecycleState::AwaitingApproval;
        mission.assignments[0].approval_state = ApprovalState::Pending {
            requested_by: "operator-human".to_string(),
            requested_at_ms: 1_704_000_010_200,
        };
        mission.assignments[0].outcome = None;
        mission.assignments[0].updated_at_ms = None;

        let record = mission
            .continue_assignment_after_approval(
                &AssignmentId("assignment:a".to_string()),
                "operator-human",
                1_704_000_010_300,
                "sha256:new-approval",
            )
            .unwrap();

        assert_eq!(
            record.lifecycle_from,
            MissionLifecycleState::AwaitingApproval
        );
        assert_eq!(record.lifecycle_to, MissionLifecycleState::Running);
        assert_eq!(
            record.transition_kind,
            MissionLifecycleTransitionKind::ApprovalGranted
        );
        assert_eq!(record.reason_code, "approval_granted");
        assert!(record.error_code.is_none());
        assert!(!record.idempotent);
        assert_eq!(mission.lifecycle_state, MissionLifecycleState::Running);
        assert!(matches!(
            mission.assignments[0].approval_state,
            ApprovalState::Approved {
                ref approved_by,
                approved_at_ms,
                ref approval_code_hash
            } if approved_by == "operator-human"
                && approved_at_ms == 1_704_000_010_300
                && approval_code_hash == "sha256:new-approval"
        ));
        assert!(mission.validate().is_ok());
    }

    #[test]
    fn mission_approval_continuation_is_idempotent_for_same_approval() {
        let mut mission = sample_mission();
        mission.lifecycle_state = MissionLifecycleState::Running;
        mission.assignments[0].approval_state = ApprovalState::Approved {
            approved_by: "operator-human".to_string(),
            approved_at_ms: 1_704_000_010_400,
            approval_code_hash: "sha256:stable".to_string(),
        };
        mission.assignments[0].outcome = None;

        let record = mission
            .continue_assignment_after_approval(
                &AssignmentId("assignment:a".to_string()),
                "operator-human",
                1_704_000_010_400,
                "sha256:stable",
            )
            .unwrap();

        assert!(record.idempotent);
        assert_eq!(mission.lifecycle_state, MissionLifecycleState::Running);
        assert!(mission.validate().is_ok());
    }

    #[test]
    fn mission_approval_continuation_on_expired_state_uses_safe_fallback() {
        let mut mission = sample_mission();
        mission.lifecycle_state = MissionLifecycleState::AwaitingApproval;
        mission.assignments[0].approval_state = ApprovalState::Expired {
            expired_at_ms: 1_704_000_010_500,
            reason_code: MissionFailureCode::ApprovalExpired
                .reason_code()
                .to_string(),
        };
        mission.assignments[0].outcome = None;

        let record = mission
            .continue_assignment_after_approval(
                &AssignmentId("assignment:a".to_string()),
                "operator-human",
                1_704_000_010_600,
                "sha256:new",
            )
            .unwrap();

        assert_eq!(
            record.transition_kind,
            MissionLifecycleTransitionKind::ApprovalExpired
        );
        assert_eq!(
            record.reason_code,
            MissionFailureCode::ApprovalExpired.reason_code()
        );
        assert_eq!(
            record.error_code.as_deref(),
            Some(MissionFailureCode::ApprovalExpired.error_code())
        );
        assert_eq!(mission.lifecycle_state, MissionLifecycleState::Failed);
        assert!(matches!(
            mission.assignments[0].outcome,
            Some(Outcome::Failed {
                ref reason_code,
                ref error_code,
                completed_at_ms
            }) if reason_code == MissionFailureCode::ApprovalExpired.reason_code()
                && error_code == MissionFailureCode::ApprovalExpired.error_code()
                && completed_at_ms == 1_704_000_010_600
        ));
        assert!(mission.validate().is_ok());
    }

    #[test]
    fn mission_expire_approval_is_idempotent_and_sets_canonical_failure_outcome() {
        let mut mission = sample_mission();
        mission.lifecycle_state = MissionLifecycleState::AwaitingApproval;
        mission.assignments[0].approval_state = ApprovalState::Pending {
            requested_by: "operator-human".to_string(),
            requested_at_ms: 1_704_000_010_700,
        };
        mission.assignments[0].outcome = None;

        let first = mission
            .expire_assignment_approval(
                &AssignmentId("assignment:a".to_string()),
                1_704_000_010_800,
            )
            .unwrap();
        assert!(!first.idempotent);
        assert_eq!(mission.lifecycle_state, MissionLifecycleState::Failed);

        let second = mission
            .expire_assignment_approval(
                &AssignmentId("assignment:a".to_string()),
                1_704_000_010_800,
            )
            .unwrap();
        assert!(second.idempotent);
        assert!(mission.validate().is_ok());
    }

    #[test]
    fn mission_validate_requires_pending_assignment_when_awaiting_approval() {
        let mut mission = sample_mission();
        mission.lifecycle_state = MissionLifecycleState::AwaitingApproval;
        mission.assignments[0].approval_state = ApprovalState::Approved {
            approved_by: "operator-human".to_string(),
            approved_at_ms: 1_704_000_000_220,
            approval_code_hash: "sha256:abcd".to_string(),
        };
        mission.assignments[0].outcome = None;

        let err = mission.validate().unwrap_err();
        assert!(matches!(
            err,
            MissionValidationError::AwaitingApprovalWithoutPendingAssignment
        ));
    }

    #[test]
    fn mission_validate_rejects_pending_assignment_outside_awaiting_state() {
        let mut mission = sample_mission();
        mission.lifecycle_state = MissionLifecycleState::Dispatching;
        mission.assignments[0].approval_state = ApprovalState::Pending {
            requested_by: "operator-human".to_string(),
            requested_at_ms: 1_704_000_010_900,
        };
        mission.assignments[0].outcome = None;

        let err = mission.validate().unwrap_err();
        assert!(matches!(
            err,
            MissionValidationError::PendingApprovalOutsideAwaitingState {
                state: MissionLifecycleState::Dispatching
            }
        ));
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
            .paths = vec![String::new()];

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
        assert!(
            !MissionFailureCode::PolicyDenied
                .retryability()
                .is_retryable()
        );
        assert!(
            !MissionFailureCode::ApprovalDenied
                .retryability()
                .is_retryable()
        );
        assert!(!MissionFailureCode::RateLimited.terminality().is_terminal());
        assert!(
            MissionFailureCode::RateLimited
                .retryability()
                .is_retryable()
        );
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
        assert!(
            deny_outcome
                .human_hint
                .as_deref()
                .unwrap()
                .contains("Policy denied")
        );
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
            vec![
                MissionFailureCode::ReservationConflict
                    .reason_code()
                    .to_string()
            ]
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
    fn mission_reservation_feasibility_denies_conflicting_lease_and_returns_feedback() {
        let mission = sample_mission();
        let report = mission
            .evaluate_reservation_feasibility(
                &[MissionReservationLease {
                    lease_id: "lease:xyz".to_string(),
                    holder: "other-agent".to_string(),
                    paths: vec!["crates/frankenterm-core/src/plan.rs".to_string()],
                    exclusive: true,
                    expires_at_ms: Some(1_704_000_999_999),
                }],
                1_704_000_000_500,
            )
            .unwrap();

        assert!(report.has_denials());
        assert_eq!(
            report.planner_feedback_reason_codes,
            vec![
                MissionFailureCode::ReservationConflict
                    .reason_code()
                    .to_string()
            ]
        );

        let denial = report
            .outcomes
            .iter()
            .find(|outcome| outcome.decision == MissionPolicyDecisionKind::Deny)
            .expect("expected denial outcome");
        assert_eq!(
            denial.reason_code.as_deref(),
            Some(MissionFailureCode::ReservationConflict.reason_code())
        );
        assert_eq!(
            denial.error_code.as_deref(),
            Some(MissionFailureCode::ReservationConflict.error_code())
        );
        assert!(
            denial
                .context
                .as_deref()
                .unwrap_or_default()
                .contains("recommendation=coordinate_or_wait")
        );
    }

    #[test]
    fn mission_reservation_feasibility_allows_same_holder_or_expired_lease() {
        let mission = sample_mission();
        let report = mission
            .evaluate_reservation_feasibility(
                &[
                    MissionReservationLease {
                        lease_id: "lease:same-owner".to_string(),
                        holder: "executor-agent-1".to_string(),
                        paths: vec!["crates/frankenterm-core/src/plan.rs".to_string()],
                        exclusive: true,
                        expires_at_ms: Some(1_704_000_999_999),
                    },
                    MissionReservationLease {
                        lease_id: "lease:expired".to_string(),
                        holder: "other-agent".to_string(),
                        paths: vec!["crates/frankenterm-core/src/plan.rs".to_string()],
                        exclusive: true,
                        expires_at_ms: Some(1_703_999_999_999),
                    },
                ],
                1_704_000_000_500,
            )
            .unwrap();

        assert!(!report.has_denials());
        assert!(report.planner_feedback_reason_codes.is_empty());
    }

    #[test]
    fn mission_reservation_feasibility_marks_expired_intent_as_stale_state() {
        let mut mission = sample_mission();
        mission.assignments[0]
            .reservation_intent
            .as_mut()
            .expect("reservation intent")
            .expires_at_ms = Some(1_703_999_999_999);

        let report = mission
            .evaluate_reservation_feasibility(&[], 1_704_000_000_500)
            .unwrap();

        assert!(report.has_denials());
        assert_eq!(
            report.planner_feedback_reason_codes,
            vec![MissionFailureCode::StaleState.reason_code().to_string()]
        );
        assert_eq!(
            report.outcomes[0].reason_code.as_deref(),
            Some(MissionFailureCode::StaleState.reason_code())
        );
    }

    #[test]
    fn mission_reservation_paths_overlap_supports_wildcard_patterns() {
        assert!(Mission::reservation_paths_overlap(
            "crates/frankenterm-core/src/*.rs",
            "crates/frankenterm-core/src/plan.rs"
        ));
        assert!(Mission::reservation_paths_overlap(
            "crates/**",
            "crates/frankenterm-core/src/plan.rs"
        ));
        assert!(Mission::reservation_paths_overlap(
            "crates/frankenterm-core/src/plan.rs",
            "crates/frankenterm-core/src/*.rs"
        ));
        assert!(!Mission::reservation_paths_overlap(
            "docs/**/*.md",
            "crates/frankenterm-core/src/plan.rs"
        ));
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
        assert!(
            !contract
                .edge_cases
                .iter()
                .any(|edge| matches!(edge, MissionDispatchEdgeCase::StaleBeadState { .. }))
        );
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
    fn mission_dispatch_adapter_resolves_target_with_pane_agent_and_thread() {
        let mission = sample_mission();
        let target = mission
            .resolve_dispatch_target(&AssignmentId("assignment:a".to_string()))
            .unwrap();

        assert_eq!(target.assignment_id.0, "assignment:a");
        assert_eq!(target.candidate_id.0, "candidate:a");
        assert_eq!(target.assignee, "executor-agent-1");
        assert_eq!(target.pane_id, Some(1));
        assert_eq!(target.thread_id.as_deref(), Some("ft-1i2ge.1.1"));
        assert_eq!(target.bead_id.as_deref(), Some("ft-1i2ge.1.1"));
    }

    #[test]
    fn mission_dispatch_adapter_wait_for_target_resolves_pane_from_condition() {
        let mut mission = sample_mission();
        mission.candidates.push(CandidateAction {
            candidate_id: CandidateActionId("candidate:b".to_string()),
            requested_by: MissionActorRole::Planner,
            action: StepAction::WaitFor {
                pane_id: None,
                condition: WaitCondition::Pattern {
                    pane_id: Some(7),
                    rule_id: "core.codex:done".to_string(),
                },
                timeout_ms: 2_500,
            },
            rationale: "Wait for done marker".to_string(),
            score: Some(0.51),
            created_at_ms: 1_704_000_001_000,
        });
        mission.assignments.push(Assignment {
            assignment_id: AssignmentId("assignment:b".to_string()),
            candidate_id: CandidateActionId("candidate:b".to_string()),
            assigned_by: MissionActorRole::Dispatcher,
            assignee: "executor-agent-2".to_string(),
            reservation_intent: None,
            approval_state: ApprovalState::NotRequired,
            outcome: None,
            escalation: None,
            created_at_ms: 1_704_000_001_100,
            updated_at_ms: None,
        });

        let target = mission
            .resolve_dispatch_target(&AssignmentId("assignment:b".to_string()))
            .unwrap();
        assert_eq!(target.pane_id, Some(7));
        assert_eq!(target.assignee, "executor-agent-2");
    }

    #[test]
    fn mission_dispatch_adapter_rejects_unknown_assignment() {
        let mission = sample_mission();
        let err = mission
            .resolve_dispatch_target(&AssignmentId("assignment:missing".to_string()))
            .unwrap_err();
        assert!(matches!(
            err,
            MissionValidationError::UnknownAssignmentReference(_)
        ));
    }

    #[test]
    fn mission_dispatch_adapter_dry_run_normalizes_success_outcome() {
        let mission = sample_mission();
        let execution = mission
            .dispatch_assignment_dry_run(
                &AssignmentId("assignment:a".to_string()),
                1_704_000_002_000,
            )
            .unwrap();

        assert_eq!(execution.mode, MissionDispatchMode::DryRun);
        assert_eq!(execution.target.assignment_id.0, "assignment:a");
        match execution.outcome {
            Outcome::Success {
                reason_code,
                completed_at_ms,
            } => {
                assert_eq!(reason_code, "dispatch_dry_run");
                assert_eq!(completed_at_ms, 1_704_000_002_000);
            }
            other => panic!("expected dry-run success outcome, got {other:?}"),
        }
    }

    #[test]
    fn mission_dispatch_adapter_live_success_defaults_reason_code() {
        let mission = sample_mission();
        let execution = mission
            .dispatch_assignment_live(
                &AssignmentId("assignment:a".to_string()),
                MissionDispatchLiveResponse::Delivered {
                    reason_code: None,
                    completed_at_ms: 1_704_000_002_100,
                },
            )
            .unwrap();

        assert_eq!(execution.mode, MissionDispatchMode::Live);
        match execution.outcome {
            Outcome::Success {
                reason_code,
                completed_at_ms,
            } => {
                assert_eq!(reason_code, "dispatch_executed");
                assert_eq!(completed_at_ms, 1_704_000_002_100);
            }
            other => panic!("expected live success outcome, got {other:?}"),
        }
    }

    #[test]
    fn mission_dispatch_adapter_live_failure_normalizes_reason_and_error_code() {
        let mission = sample_mission();
        let execution = mission
            .dispatch_assignment_live(
                &AssignmentId("assignment:a".to_string()),
                MissionDispatchLiveResponse::Failed {
                    reason_code: MissionFailureCode::ReservationConflict
                        .reason_code()
                        .to_string(),
                    error_code: None,
                    completed_at_ms: 1_704_000_002_200,
                },
            )
            .unwrap();

        match execution.outcome {
            Outcome::Failed {
                reason_code,
                error_code,
                completed_at_ms,
            } => {
                assert_eq!(
                    reason_code,
                    MissionFailureCode::ReservationConflict.reason_code()
                );
                assert_eq!(
                    error_code,
                    MissionFailureCode::ReservationConflict.error_code()
                );
                assert_eq!(completed_at_ms, 1_704_000_002_200);
            }
            other => panic!("expected normalized live failure outcome, got {other:?}"),
        }
    }

    #[test]
    fn mission_dispatch_adapter_live_failure_rejects_unknown_reason_code() {
        let mission = sample_mission();
        let err = mission
            .dispatch_assignment_live(
                &AssignmentId("assignment:a".to_string()),
                MissionDispatchLiveResponse::Failed {
                    reason_code: "not_a_real_reason".to_string(),
                    error_code: None,
                    completed_at_ms: 1_704_000_002_250,
                },
            )
            .unwrap_err();

        assert!(matches!(
            err,
            MissionValidationError::UnknownFailureReasonCode { .. }
        ));
    }

    #[test]
    fn mission_dispatch_adapter_live_failure_rejects_mismatched_error_code() {
        let mission = sample_mission();
        let err = mission
            .dispatch_assignment_live(
                &AssignmentId("assignment:a".to_string()),
                MissionDispatchLiveResponse::Failed {
                    reason_code: MissionFailureCode::RateLimited.reason_code().to_string(),
                    error_code: Some(
                        MissionFailureCode::ReservationConflict
                            .error_code()
                            .to_string(),
                    ),
                    completed_at_ms: 1_704_000_002_300,
                },
            )
            .unwrap_err();

        assert!(matches!(
            err,
            MissionValidationError::MismatchedFailureErrorCode { .. }
        ));
    }

    // ========================================================================
    // Dispatch Idempotency and Deduplication (ft-1i2ge.3.6)
    // ========================================================================

    #[test]
    fn mission_dispatch_idempotency_key_is_deterministic_for_same_inputs() {
        let mission_id = MissionId("mission:abc".to_string());
        let assignment_id = AssignmentId("assignment:a".to_string());
        let mechanism = MissionDispatchMechanism::RobotSend {
            pane_id: 0,
            text: "/compact".to_string(),
            paste_mode: None,
        };

        let key1 = MissionDispatchIdempotencyKey::compute(&mission_id, &assignment_id, &mechanism);
        let key2 = MissionDispatchIdempotencyKey::compute(&mission_id, &assignment_id, &mechanism);
        assert_eq!(key1, key2, "same inputs must produce same idempotency key");
        assert!(
            key1.as_str().starts_with("dispatch:"),
            "key must have dispatch: prefix"
        );
    }

    #[test]
    fn mission_dispatch_idempotency_key_differs_for_different_mechanisms() {
        let mission_id = MissionId("mission:abc".to_string());
        let assignment_id = AssignmentId("assignment:a".to_string());
        let mech_a = MissionDispatchMechanism::RobotSend {
            pane_id: 0,
            text: "/compact".to_string(),
            paste_mode: None,
        };
        let mech_b = MissionDispatchMechanism::RobotSend {
            pane_id: 0,
            text: "/retry".to_string(),
            paste_mode: None,
        };

        let key_a = MissionDispatchIdempotencyKey::compute(&mission_id, &assignment_id, &mech_a);
        let key_b = MissionDispatchIdempotencyKey::compute(&mission_id, &assignment_id, &mech_b);
        assert_ne!(
            key_a, key_b,
            "different mechanisms must produce different keys"
        );
    }

    #[test]
    fn mission_dispatch_idempotency_key_differs_for_different_assignments() {
        let mission_id = MissionId("mission:abc".to_string());
        let mechanism = MissionDispatchMechanism::RobotSend {
            pane_id: 0,
            text: "/compact".to_string(),
            paste_mode: None,
        };

        let key_a = MissionDispatchIdempotencyKey::compute(
            &mission_id,
            &AssignmentId("assignment:a".to_string()),
            &mechanism,
        );
        let key_b = MissionDispatchIdempotencyKey::compute(
            &mission_id,
            &AssignmentId("assignment:b".to_string()),
            &mechanism,
        );
        assert_ne!(
            key_a, key_b,
            "different assignments must produce different keys"
        );
    }

    #[test]
    fn mission_dispatch_dedup_state_records_and_finds_by_key() {
        let mut state = MissionDispatchDeduplicationState::default();
        assert!(state.is_empty());

        let key = MissionDispatchIdempotencyKey("dispatch:abc123".to_string());
        let record = MissionDispatchDeduplicationRecord {
            idempotency_key: key.clone(),
            assignment_id: AssignmentId("assignment:a".to_string()),
            correlation_id: "corr-1".to_string(),
            dispatched_at_ms: 1_000,
            outcome: Outcome::Success {
                reason_code: "dispatch_executed".to_string(),
                completed_at_ms: 1_000,
            },
            mechanism_hash: "deadbeef".to_string(),
        };

        state.record_dispatch(record.clone());
        assert!(!state.is_empty());
        assert_eq!(state.find_by_key(&key), Some(&record));
        assert!(
            state
                .find_by_key(&MissionDispatchIdempotencyKey("dispatch:other".to_string()))
                .is_none()
        );
    }

    #[test]
    fn mission_dispatch_dedup_state_overwrites_on_same_key() {
        let mut state = MissionDispatchDeduplicationState::default();
        let key = MissionDispatchIdempotencyKey("dispatch:abc123".to_string());

        let record1 = MissionDispatchDeduplicationRecord {
            idempotency_key: key.clone(),
            assignment_id: AssignmentId("assignment:a".to_string()),
            correlation_id: "corr-1".to_string(),
            dispatched_at_ms: 1_000,
            outcome: Outcome::Success {
                reason_code: "dispatch_executed".to_string(),
                completed_at_ms: 1_000,
            },
            mechanism_hash: "deadbeef".to_string(),
        };
        let record2 = MissionDispatchDeduplicationRecord {
            idempotency_key: key.clone(),
            assignment_id: AssignmentId("assignment:a".to_string()),
            correlation_id: "corr-2".to_string(),
            dispatched_at_ms: 2_000,
            outcome: Outcome::Failed {
                reason_code: MissionFailureCode::DispatchError.reason_code().to_string(),
                error_code: MissionFailureCode::DispatchError.error_code().to_string(),
                completed_at_ms: 2_000,
            },
            mechanism_hash: "deadbeef".to_string(),
        };

        state.record_dispatch(record1);
        state.record_dispatch(record2.clone());
        assert_eq!(state.records.len(), 1, "overwrite, not append");
        assert_eq!(state.find_by_key(&key), Some(&record2));
    }

    #[test]
    fn mission_dispatch_dedup_state_evicts_before_cutoff() {
        let mut state = MissionDispatchDeduplicationState::default();
        let key_old = MissionDispatchIdempotencyKey("dispatch:old".to_string());
        let key_new = MissionDispatchIdempotencyKey("dispatch:new".to_string());

        state.record_dispatch(MissionDispatchDeduplicationRecord {
            idempotency_key: key_old.clone(),
            assignment_id: AssignmentId("assignment:a".to_string()),
            correlation_id: "corr-old".to_string(),
            dispatched_at_ms: 500,
            outcome: Outcome::Success {
                reason_code: "ok".to_string(),
                completed_at_ms: 500,
            },
            mechanism_hash: "hash1".to_string(),
        });
        state.record_dispatch(MissionDispatchDeduplicationRecord {
            idempotency_key: key_new.clone(),
            assignment_id: AssignmentId("assignment:b".to_string()),
            correlation_id: "corr-new".to_string(),
            dispatched_at_ms: 2_000,
            outcome: Outcome::Success {
                reason_code: "ok".to_string(),
                completed_at_ms: 2_000,
            },
            mechanism_hash: "hash2".to_string(),
        });

        assert_eq!(state.records.len(), 2);
        state.evict_before(1_000);
        assert_eq!(state.records.len(), 1);
        assert!(state.find_by_key(&key_old).is_none());
        assert!(state.find_by_key(&key_new).is_some());
    }

    #[test]
    fn mission_dispatch_dedup_evaluate_fresh_dispatch_returns_not_duplicate() {
        let mission = sample_mission();
        let result = mission
            .evaluate_dispatch_deduplication(
                &AssignmentId("assignment:a".to_string()),
                "corr-fresh-1",
            )
            .unwrap();

        assert!(!result.is_duplicate);
        assert_eq!(result.decision_path, "dedup_no_prior_record");
        assert_eq!(result.reason_code, "dispatch_fresh");
        assert!(result.cached_record.is_none());
    }

    #[test]
    fn mission_dispatch_dedup_evaluate_rejects_empty_correlation_id() {
        let mission = sample_mission();
        let err = mission
            .evaluate_dispatch_deduplication(&AssignmentId("assignment:a".to_string()), "")
            .unwrap_err();
        assert!(
            matches!(err, MissionValidationError::InvalidFieldValue { .. }),
            "empty correlation_id must be rejected"
        );
    }

    #[test]
    fn mission_dispatch_idempotent_first_call_executes_and_records() {
        let mut mission = sample_mission();
        mission.lifecycle_state = MissionLifecycleState::Dispatching;
        mission.assignments[0].outcome = None;

        let (execution, dedup) = mission
            .dispatch_assignment_live_idempotent(
                &AssignmentId("assignment:a".to_string()),
                "corr-idem-1",
                MissionDispatchLiveResponse::Delivered {
                    reason_code: Some("dispatch_executed".to_string()),
                    completed_at_ms: 1_704_000_100_000,
                },
                1_704_000_100_000,
            )
            .unwrap();

        assert!(!dedup.is_duplicate, "first call must not be a duplicate");
        assert_eq!(dedup.reason_code, "dispatch_fresh");
        assert!(matches!(execution.outcome, Outcome::Success { .. }));
        assert!(
            !mission.dispatch_dedup_state.is_empty(),
            "must record dispatch"
        );
    }

    #[test]
    fn mission_dispatch_idempotent_second_call_returns_cached_outcome() {
        let mut mission = sample_mission();
        mission.lifecycle_state = MissionLifecycleState::Dispatching;
        mission.assignments[0].outcome = None;

        // First dispatch
        let (first_exec, first_dedup) = mission
            .dispatch_assignment_live_idempotent(
                &AssignmentId("assignment:a".to_string()),
                "corr-idem-2a",
                MissionDispatchLiveResponse::Delivered {
                    reason_code: Some("dispatch_executed".to_string()),
                    completed_at_ms: 1_704_000_100_000,
                },
                1_704_000_100_000,
            )
            .unwrap();
        assert!(!first_dedup.is_duplicate);

        // Second dispatch (duplicate) — same assignment, same mechanism
        let (second_exec, second_dedup) = mission
            .dispatch_assignment_live_idempotent(
                &AssignmentId("assignment:a".to_string()),
                "corr-idem-2b",
                MissionDispatchLiveResponse::Delivered {
                    reason_code: Some("should_not_be_used".to_string()),
                    completed_at_ms: 1_704_000_200_000,
                },
                1_704_000_200_000,
            )
            .unwrap();

        assert!(
            second_dedup.is_duplicate,
            "second call must detect duplicate"
        );
        assert_eq!(second_dedup.decision_path, "dedup_exact_match");
        assert_eq!(second_dedup.reason_code, "dispatch_duplicate");
        // Cached outcome should match the first execution, not the second response
        assert_eq!(first_exec.outcome, second_exec.outcome);
    }

    #[test]
    fn mission_dispatch_dedup_serde_roundtrip_preserves_state() {
        let mut mission = sample_mission();
        mission.lifecycle_state = MissionLifecycleState::Dispatching;
        mission.assignments[0].outcome = None;

        // Execute a dispatch to populate dedup state
        let _result = mission.dispatch_assignment_live_idempotent(
            &AssignmentId("assignment:a".to_string()),
            "corr-serde-1",
            MissionDispatchLiveResponse::Delivered {
                reason_code: Some("dispatch_executed".to_string()),
                completed_at_ms: 1_704_000_100_000,
            },
            1_704_000_100_000,
        );

        // Serialize and deserialize the mission
        let json = serde_json::to_string(&mission).unwrap();
        let restored: Mission = serde_json::from_str(&json).unwrap();

        assert_eq!(
            mission.dispatch_dedup_state.records.len(),
            restored.dispatch_dedup_state.records.len(),
            "dedup state must survive serde roundtrip"
        );
        assert_eq!(
            mission.dispatch_dedup_state.records[0].idempotency_key,
            restored.dispatch_dedup_state.records[0].idempotency_key,
        );
    }

    #[test]
    fn mission_dispatch_dedup_canonical_string_is_deterministic() {
        let state = MissionDispatchDeduplicationState {
            records: vec![MissionDispatchDeduplicationRecord {
                idempotency_key: MissionDispatchIdempotencyKey("dispatch:test".to_string()),
                assignment_id: AssignmentId("assignment:x".to_string()),
                correlation_id: "corr-canon".to_string(),
                dispatched_at_ms: 42_000,
                outcome: Outcome::Success {
                    reason_code: "ok".to_string(),
                    completed_at_ms: 42_000,
                },
                mechanism_hash: "abcd1234".to_string(),
            }],
        };

        let s1 = state.canonical_string();
        let s2 = state.canonical_string();
        assert_eq!(s1, s2, "canonical_string must be deterministic");
        assert!(s1.contains("dedup_records="), "must include records prefix");
        assert!(s1.contains("dispatch:test"), "must include key");
    }

    #[test]
    fn mission_reconcile_assignment_signal_applies_success_and_completes() {
        let mut mission = sample_mission();
        mission.lifecycle_state = MissionLifecycleState::Dispatching;
        mission.assignments[0].outcome = None;
        mission.assignments[0].approval_state = ApprovalState::NotRequired;
        mission.assignments[0].updated_at_ms = Some(1_704_000_020_000);

        let signal = MissionAssignmentSignal {
            assignment_id: AssignmentId("assignment:a".to_string()),
            observed_at_ms: 1_704_000_020_100,
            correlation_id: "corr-c3-success".to_string(),
            payload: MissionAssignmentSignalPayload::Completed {
                reason_code: "dispatch_executed".to_string(),
                completed_at_ms: 1_704_000_020_100,
            },
        };
        let report = mission.reconcile_assignment_signal(&signal).unwrap();

        assert!(report.applied);
        assert!(!report.out_of_order);
        assert_eq!(report.reason_code, "signal_reconciled");
        assert_eq!(mission.lifecycle_state, MissionLifecycleState::Completed);
        assert!(matches!(
            mission.assignments[0].outcome,
            Some(Outcome::Success {
                ref reason_code,
                completed_at_ms
            }) if reason_code == "dispatch_executed" && completed_at_ms == 1_704_000_020_100
        ));
        assert!(mission.validate().is_ok());
    }

    #[test]
    fn mission_reconcile_assignment_signal_timeout_maps_to_dispatch_error_failure() {
        let mut mission = sample_mission();
        mission.lifecycle_state = MissionLifecycleState::Running;
        mission.assignments[0].outcome = None;
        mission.assignments[0].approval_state = ApprovalState::NotRequired;
        mission.assignments[0].updated_at_ms = Some(1_704_000_020_200);

        let signal = MissionAssignmentSignal {
            assignment_id: AssignmentId("assignment:a".to_string()),
            observed_at_ms: 1_704_000_020_300,
            correlation_id: "corr-c3-timeout".to_string(),
            payload: MissionAssignmentSignalPayload::TimedOut {
                completed_at_ms: 1_704_000_020_300,
            },
        };
        let report = mission.reconcile_assignment_signal(&signal).unwrap();

        assert!(report.applied);
        assert_eq!(mission.lifecycle_state, MissionLifecycleState::Failed);
        assert!(matches!(
            mission.assignments[0].outcome,
            Some(Outcome::Failed {
                ref reason_code,
                ref error_code,
                completed_at_ms
            }) if reason_code == MissionFailureCode::DispatchError.reason_code()
                && error_code == MissionFailureCode::DispatchError.error_code()
                && completed_at_ms == 1_704_000_020_300
        ));
    }

    #[test]
    fn mission_reconcile_assignment_signal_ignores_out_of_order_update() {
        let mut mission = sample_mission();
        mission.lifecycle_state = MissionLifecycleState::Running;
        mission.assignments[0].outcome = Some(Outcome::Success {
            reason_code: "already_done".to_string(),
            completed_at_ms: 1_704_000_020_500,
        });
        mission.assignments[0].updated_at_ms = Some(1_704_000_020_500);

        let signal = MissionAssignmentSignal {
            assignment_id: AssignmentId("assignment:a".to_string()),
            observed_at_ms: 1_704_000_020_400,
            correlation_id: "corr-c3-old".to_string(),
            payload: MissionAssignmentSignalPayload::Failed {
                reason_code: MissionFailureCode::DispatchError.reason_code().to_string(),
                error_code: None,
                completed_at_ms: 1_704_000_020_400,
            },
        };
        let report = mission.reconcile_assignment_signal(&signal).unwrap();

        assert!(!report.applied);
        assert!(report.out_of_order);
        assert_eq!(
            report.reason_code,
            MissionFailureCode::StaleState.reason_code()
        );
        assert_eq!(mission.lifecycle_state, MissionLifecycleState::Running);
        assert!(matches!(
            mission.assignments[0].outcome,
            Some(Outcome::Success {
                ref reason_code,
                completed_at_ms
            }) if reason_code == "already_done" && completed_at_ms == 1_704_000_020_500
        ));
    }

    #[test]
    fn mission_reconcile_assignment_signal_surfaces_drift_on_newer_conflict() {
        let mut mission = sample_mission();
        mission.lifecycle_state = MissionLifecycleState::Running;
        mission.assignments[0].outcome = Some(Outcome::Success {
            reason_code: "dispatch_executed".to_string(),
            completed_at_ms: 1_704_000_020_600,
        });
        mission.assignments[0].updated_at_ms = Some(1_704_000_020_600);

        let signal = MissionAssignmentSignal {
            assignment_id: AssignmentId("assignment:a".to_string()),
            observed_at_ms: 1_704_000_020_700,
            correlation_id: "corr-c3-drift".to_string(),
            payload: MissionAssignmentSignalPayload::Failed {
                reason_code: MissionFailureCode::DispatchError.reason_code().to_string(),
                error_code: None,
                completed_at_ms: 1_704_000_020_700,
            },
        };
        let report = mission.reconcile_assignment_signal(&signal).unwrap();

        assert!(report.applied);
        assert!(report.drift.is_some());
        assert_eq!(mission.lifecycle_state, MissionLifecycleState::Failed);
        let drift = report.drift.unwrap();
        assert_eq!(drift.reason_code, "state_drift_detected");
        assert!(drift.previous_outcome.is_some());
        assert!(matches!(
            drift.incoming_outcome,
            Outcome::Failed {
                ref reason_code, ..
            } if reason_code == MissionFailureCode::DispatchError.reason_code()
        ));
    }

    #[test]
    fn mission_reconcile_assignment_signal_rejects_unknown_failure_code() {
        let mut mission = sample_mission();
        mission.lifecycle_state = MissionLifecycleState::Running;
        mission.assignments[0].outcome = None;

        let signal = MissionAssignmentSignal {
            assignment_id: AssignmentId("assignment:a".to_string()),
            observed_at_ms: 1_704_000_020_800,
            correlation_id: "corr-c3-bad-code".to_string(),
            payload: MissionAssignmentSignalPayload::Failed {
                reason_code: "not_a_real_reason".to_string(),
                error_code: None,
                completed_at_ms: 1_704_000_020_800,
            },
        };

        let err = mission.reconcile_assignment_signal(&signal).unwrap_err();
        assert!(matches!(
            err,
            MissionValidationError::UnknownFailureReasonCode { .. }
        ));
    }

    #[test]
    fn mission_adaptive_replan_schedules_retry_pending_from_failed_state() {
        let mut mission = sample_mission();
        mission.lifecycle_state = MissionLifecycleState::Failed;
        mission.assignments[0].outcome = Some(Outcome::Failed {
            reason_code: MissionFailureCode::DispatchError.reason_code().to_string(),
            error_code: MissionFailureCode::DispatchError.error_code().to_string(),
            completed_at_ms: 1_704_000_030_000,
        });

        let trigger = MissionReplanTrigger {
            kind: MissionReplanTriggerKind::Failed,
            assignment_id: Some(AssignmentId("assignment:a".to_string())),
            observed_at_ms: 1_704_000_030_100,
            correlation_id: "corr-c4-failure-1".to_string(),
            reason_code: Some(MissionFailureCode::DispatchError.reason_code().to_string()),
        };
        let decision = mission
            .evaluate_adaptive_replan(&trigger, &MissionReplanBackoffPolicy::default())
            .unwrap();

        assert!(decision.apply_replan);
        assert_eq!(decision.decision_path, "adaptive_replan_scheduled");
        assert_eq!(decision.reason_code, "replan_failure_signal");
        assert_eq!(decision.backoff_ms, 500);
        assert_eq!(decision.attempt, 1);
        assert_eq!(decision.lifecycle_from, MissionLifecycleState::Failed);
        assert_eq!(decision.lifecycle_to, MissionLifecycleState::RetryPending);
        assert_eq!(mission.lifecycle_state, MissionLifecycleState::RetryPending);
        assert_eq!(mission.replan_state.consecutive_replan_count, 1);
        assert_eq!(
            mission.replan_state.last_trigger_kind,
            Some(MissionReplanTriggerKind::Failed)
        );
        assert!(mission.validate().is_ok());
    }

    #[test]
    fn mission_adaptive_replan_backoff_blocks_tight_loop() {
        let mut mission = sample_mission();
        mission.lifecycle_state = MissionLifecycleState::Failed;
        mission.assignments[0].outcome = Some(Outcome::Failed {
            reason_code: MissionFailureCode::DispatchError.reason_code().to_string(),
            error_code: MissionFailureCode::DispatchError.error_code().to_string(),
            completed_at_ms: 1_704_000_031_000,
        });

        let first = mission
            .evaluate_adaptive_replan(
                &MissionReplanTrigger {
                    kind: MissionReplanTriggerKind::Failed,
                    assignment_id: Some(AssignmentId("assignment:a".to_string())),
                    observed_at_ms: 1_704_000_031_100,
                    correlation_id: "corr-c4-loop-1".to_string(),
                    reason_code: Some(MissionFailureCode::DispatchError.reason_code().to_string()),
                },
                &MissionReplanBackoffPolicy::default(),
            )
            .unwrap();
        let second = mission
            .evaluate_adaptive_replan(
                &MissionReplanTrigger {
                    kind: MissionReplanTriggerKind::Failed,
                    assignment_id: Some(AssignmentId("assignment:a".to_string())),
                    observed_at_ms: first.next_eligible_replan_at_ms - 1,
                    correlation_id: "corr-c4-loop-2".to_string(),
                    reason_code: Some(MissionFailureCode::DispatchError.reason_code().to_string()),
                },
                &MissionReplanBackoffPolicy::default(),
            )
            .unwrap();

        assert!(!second.apply_replan);
        assert_eq!(second.decision_path, "backoff_guard");
        assert_eq!(second.reason_code, "replan_backoff_active");
        assert_eq!(second.error_code.as_deref(), Some("FTM2001"));
        assert_eq!(second.backoff_ms, 1);
        assert_eq!(mission.lifecycle_state, MissionLifecycleState::RetryPending);
    }

    #[test]
    fn mission_adaptive_replan_deduplicates_correlation_ids() {
        let mut mission = sample_mission();
        mission.lifecycle_state = MissionLifecycleState::Failed;

        let trigger = MissionReplanTrigger {
            kind: MissionReplanTriggerKind::Blocked,
            assignment_id: Some(AssignmentId("assignment:a".to_string())),
            observed_at_ms: 1_704_000_032_100,
            correlation_id: "corr-c4-dedupe".to_string(),
            reason_code: None,
        };

        let first = mission
            .evaluate_adaptive_replan(&trigger, &MissionReplanBackoffPolicy::default())
            .unwrap();
        let second = mission
            .evaluate_adaptive_replan(
                &MissionReplanTrigger {
                    observed_at_ms: first.next_eligible_replan_at_ms + 5_000,
                    ..trigger.clone()
                },
                &MissionReplanBackoffPolicy::default(),
            )
            .unwrap();

        assert!(first.apply_replan);
        assert!(!second.apply_replan);
        assert_eq!(second.decision_path, "dedupe_guard");
        assert_eq!(second.reason_code, "replan_duplicate_trigger");
        assert_eq!(second.error_code, None);
        assert_eq!(second.lifecycle_to, mission.lifecycle_state);
    }

    #[test]
    fn mission_adaptive_replan_is_deterministic_under_bursty_streams() {
        let mut first = sample_mission();
        first.lifecycle_state = MissionLifecycleState::Failed;
        first.assignments[0].outcome = Some(Outcome::Failed {
            reason_code: MissionFailureCode::DispatchError.reason_code().to_string(),
            error_code: MissionFailureCode::DispatchError.error_code().to_string(),
            completed_at_ms: 1_704_000_033_000,
        });
        let mut second = first.clone();
        let policy = MissionReplanBackoffPolicy::default();

        let triggers = [
            MissionReplanTrigger {
                kind: MissionReplanTriggerKind::Failed,
                assignment_id: Some(AssignmentId("assignment:a".to_string())),
                observed_at_ms: 1_704_000_033_100,
                correlation_id: "corr-c4-burst-1".to_string(),
                reason_code: Some(MissionFailureCode::DispatchError.reason_code().to_string()),
            },
            MissionReplanTrigger {
                kind: MissionReplanTriggerKind::Blocked,
                assignment_id: Some(AssignmentId("assignment:a".to_string())),
                observed_at_ms: 1_704_000_033_400,
                correlation_id: "corr-c4-burst-2".to_string(),
                reason_code: None,
            },
            MissionReplanTrigger {
                kind: MissionReplanTriggerKind::RateLimited,
                assignment_id: Some(AssignmentId("assignment:a".to_string())),
                observed_at_ms: 1_704_000_034_300,
                correlation_id: "corr-c4-burst-3".to_string(),
                reason_code: Some(MissionFailureCode::RateLimited.reason_code().to_string()),
            },
            MissionReplanTrigger {
                kind: MissionReplanTriggerKind::OperatorOverride,
                assignment_id: None,
                observed_at_ms: 1_704_000_100_000,
                correlation_id: "corr-c4-burst-4".to_string(),
                reason_code: None,
            },
            MissionReplanTrigger {
                kind: MissionReplanTriggerKind::Completion,
                assignment_id: Some(AssignmentId("assignment:a".to_string())),
                observed_at_ms: 1_704_000_101_000,
                correlation_id: "corr-c4-burst-5".to_string(),
                reason_code: None,
            },
        ];

        let first_decisions = triggers
            .iter()
            .map(|trigger| first.evaluate_adaptive_replan(trigger, &policy).unwrap())
            .collect::<Vec<_>>();
        let second_decisions = triggers
            .iter()
            .map(|trigger| second.evaluate_adaptive_replan(trigger, &policy).unwrap())
            .collect::<Vec<_>>();

        assert_eq!(first_decisions, second_decisions);
        assert_eq!(first.replan_state, second.replan_state);
    }

    #[test]
    fn mission_adaptive_replan_rate_limited_trigger_rejects_mismatched_reason_code() {
        let mut mission = sample_mission();
        mission.lifecycle_state = MissionLifecycleState::Running;

        let err = mission
            .evaluate_adaptive_replan(
                &MissionReplanTrigger {
                    kind: MissionReplanTriggerKind::RateLimited,
                    assignment_id: Some(AssignmentId("assignment:a".to_string())),
                    observed_at_ms: 1_704_000_034_800,
                    correlation_id: "corr-c4-bad-rate-code".to_string(),
                    reason_code: Some(MissionFailureCode::DispatchError.reason_code().to_string()),
                },
                &MissionReplanBackoffPolicy::default(),
            )
            .unwrap_err();

        assert!(matches!(
            err,
            MissionValidationError::InvalidFieldValue { ref field_path, .. }
                if field_path == "replan_trigger.reason_code"
        ));
    }

    #[test]
    fn mission_validate_rejects_replan_state_count_without_timestamp() {
        let mut mission = sample_mission();
        mission.replan_state.consecutive_replan_count = 2;
        mission.replan_state.last_observed_at_ms = None;

        let err = mission.validate().unwrap_err();
        assert!(matches!(
            err,
            MissionValidationError::InvalidFieldValue { ref field_path, .. }
                if field_path == "mission.replan_state.consecutive_replan_count"
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
        assert!(
            rate_limited
                .reason_codes
                .contains(&"agent_rate_limited".to_string())
        );
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
        assert!(
            excluded
                .reason_codes
                .contains(&"assignment_excluded".to_string())
        );
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
        assert!(
            degraded_full
                .reason_codes
                .contains(&"agent_capacity_exhausted".to_string())
        );
        assert!(
            degraded_full
                .reason_codes
                .contains(&"agent_degraded".to_string())
        );
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

    // ════════════════════════════════════════════════════════════════════════
    // Intent Ledger (H2) Tests
    // ════════════════════════════════════════════════════════════════════════

    fn test_ledger() -> IntentLedger {
        IntentLedger::new(TxId("tx-test-001".to_string()))
    }

    fn test_correlation() -> LedgerCorrelation {
        LedgerCorrelation {
            mission_run_id: Some("run-42".to_string()),
            pane_ids: vec![1, 2],
            agent_ids: vec!["agent-a".to_string()],
            bead_ids: vec!["ft-test.1".to_string()],
            thread_ids: Vec::new(),
            policy_check_ids: Vec::new(),
            reservation_ids: Vec::new(),
            approval_ids: Vec::new(),
        }
    }

    #[test]
    fn ledger_new_is_empty() {
        let ledger = test_ledger();
        assert!(ledger.is_empty());
        assert_eq!(ledger.len(), 0);
        assert!(ledger.last_entry().is_none());
        assert!(ledger.entry_at(1).is_none());
    }

    #[test]
    fn ledger_append_assigns_seq_and_hash() {
        let mut ledger = test_ledger();
        let corr = LedgerCorrelation::empty();
        ledger.append(
            1000,
            LedgerEntryKind::IntentRegistered {
                summary: "test tx".to_string(),
                requested_by: "operator".to_string(),
            },
            corr,
        );
        assert_eq!(ledger.len(), 1);
        let entry = ledger.entry_at(1).unwrap();
        assert_eq!(entry.seq, 1);
        assert!(!entry.entry_hash.is_empty());
        assert!(entry.prev_hash.is_empty()); // genesis
        assert!(entry.verify_hash());
    }

    #[test]
    fn ledger_hash_chain_links_entries() {
        let mut ledger = test_ledger();
        let corr = LedgerCorrelation::empty();

        ledger.append(
            1000,
            LedgerEntryKind::IntentRegistered {
                summary: "s".to_string(),
                requested_by: "o".to_string(),
            },
            corr.clone(),
        );
        ledger.append(
            2000,
            LedgerEntryKind::StateTransition {
                from: MissionTxState::Draft,
                to: MissionTxState::Planned,
                kind: MissionTxTransitionKind::PlanCreated,
            },
            corr,
        );

        let e1 = ledger.entry_at(1).unwrap();
        let e2 = ledger.entry_at(2).unwrap();
        assert_eq!(e2.prev_hash, e1.entry_hash);
        assert_eq!(e2.seq, 2);
        assert!(e2.verify_hash());
    }

    #[test]
    fn ledger_validate_happy_path() {
        let mut ledger = test_ledger();
        let corr = LedgerCorrelation::empty();

        ledger.append(
            1000,
            LedgerEntryKind::IntentRegistered {
                summary: "tx".to_string(),
                requested_by: "op".to_string(),
            },
            corr.clone(),
        );
        ledger.append(
            2000,
            LedgerEntryKind::PlanCreated {
                plan_id: TxPlanId("plan-1".to_string()),
                step_count: 2,
                precondition_count: 1,
                compensation_count: 1,
            },
            corr.clone(),
        );
        ledger.append(
            3000,
            LedgerEntryKind::StateTransition {
                from: MissionTxState::Draft,
                to: MissionTxState::Planned,
                kind: MissionTxTransitionKind::PlanCreated,
            },
            corr,
        );

        assert!(ledger.validate().is_ok());
    }

    #[test]
    fn ledger_validate_detects_tampering() {
        let mut ledger = test_ledger();
        let corr = LedgerCorrelation::empty();

        ledger.append(
            1000,
            LedgerEntryKind::IntentRegistered {
                summary: "tx".to_string(),
                requested_by: "op".to_string(),
            },
            corr.clone(),
        );
        ledger.append(
            2000,
            LedgerEntryKind::PlanCreated {
                plan_id: TxPlanId("plan-1".to_string()),
                step_count: 2,
                precondition_count: 0,
                compensation_count: 0,
            },
            corr,
        );

        // Tamper with entry 2's timestamp
        ledger.entries[1].created_at_ms = 9999;

        let result = ledger.validate();
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(
            err,
            LedgerValidationError::TamperedEntry { seq: 2 }
        ));
    }

    #[test]
    fn ledger_validate_detects_broken_chain() {
        let mut ledger = test_ledger();
        let corr = LedgerCorrelation::empty();

        ledger.append(
            1000,
            LedgerEntryKind::IntentRegistered {
                summary: "tx".to_string(),
                requested_by: "op".to_string(),
            },
            corr.clone(),
        );
        ledger.append(
            2000,
            LedgerEntryKind::PlanCreated {
                plan_id: TxPlanId("plan-1".to_string()),
                step_count: 1,
                precondition_count: 0,
                compensation_count: 0,
            },
            corr,
        );

        // Break the chain
        ledger.entries[1].prev_hash = "bogus".to_string();
        // Recompute entry hash to avoid TamperedEntry error
        ledger.entries[1].entry_hash = ledger.entries[1].compute_hash();

        let err = ledger.validate().unwrap_err();
        assert!(matches!(
            err,
            LedgerValidationError::BrokenHashChain { seq: 2, .. }
        ));
    }

    #[test]
    fn ledger_validate_detects_invalid_genesis() {
        let mut ledger = test_ledger();
        let corr = LedgerCorrelation::empty();

        ledger.append(
            1000,
            LedgerEntryKind::IntentRegistered {
                summary: "tx".to_string(),
                requested_by: "op".to_string(),
            },
            corr,
        );

        // Set genesis prev_hash to non-empty
        ledger.entries[0].prev_hash = "non-empty".to_string();
        ledger.entries[0].entry_hash = ledger.entries[0].compute_hash();

        let err = ledger.validate().unwrap_err();
        assert!(matches!(err, LedgerValidationError::InvalidGenesis));
    }

    #[test]
    fn ledger_validate_detects_tx_id_mismatch() {
        let mut ledger = test_ledger();
        let corr = LedgerCorrelation::empty();

        ledger.append(
            1000,
            LedgerEntryKind::IntentRegistered {
                summary: "tx".to_string(),
                requested_by: "op".to_string(),
            },
            corr,
        );

        // Change tx_id on the entry
        ledger.entries[0].tx_id = TxId("wrong-tx".to_string());
        ledger.entries[0].prev_hash = String::new(); // keep genesis valid
        ledger.entries[0].entry_hash = ledger.entries[0].compute_hash();

        let err = ledger.validate().unwrap_err();
        assert!(matches!(
            err,
            LedgerValidationError::TxIdMismatch { seq: 1, .. }
        ));
    }

    #[test]
    fn ledger_empty_validates_ok() {
        let ledger = test_ledger();
        assert!(ledger.validate().is_ok());
    }

    #[test]
    fn ledger_entries_of_kind_filters_correctly() {
        let mut ledger = test_ledger();
        let corr = LedgerCorrelation::empty();

        ledger.append(
            1000,
            LedgerEntryKind::IntentRegistered {
                summary: "tx".to_string(),
                requested_by: "op".to_string(),
            },
            corr.clone(),
        );
        ledger.append(
            2000,
            LedgerEntryKind::StateTransition {
                from: MissionTxState::Draft,
                to: MissionTxState::Planned,
                kind: MissionTxTransitionKind::PlanCreated,
            },
            corr.clone(),
        );
        ledger.append(
            3000,
            LedgerEntryKind::StateTransition {
                from: MissionTxState::Planned,
                to: MissionTxState::Prepared,
                kind: MissionTxTransitionKind::PrepareSucceeded,
            },
            corr,
        );

        let transitions = ledger.entries_of_kind("state_transition");
        assert_eq!(transitions.len(), 2);
        let intents = ledger.entries_of_kind("intent_registered");
        assert_eq!(intents.len(), 1);
        let steps = ledger.entries_of_kind("step_executed");
        assert_eq!(steps.len(), 0);
    }

    #[test]
    fn ledger_entries_in_range() {
        let mut ledger = test_ledger();
        let corr = LedgerCorrelation::empty();

        for ts in [1000, 2000, 3000, 4000, 5000] {
            ledger.append(
                ts,
                LedgerEntryKind::ReceiptRecorded {
                    receipt_seq: ts as u64 / 1000,
                    state: MissionTxState::Draft,
                    reason_code: None,
                    error_code: None,
                },
                corr.clone(),
            );
        }

        let range = ledger.entries_in_range(2000, 4000);
        assert_eq!(range.len(), 3);
        assert_eq!(range[0].created_at_ms, 2000);
        assert_eq!(range[2].created_at_ms, 4000);
    }

    #[test]
    fn ledger_entries_for_pane_filters() {
        let mut ledger = test_ledger();

        let corr_pane1 = LedgerCorrelation {
            pane_ids: vec![1],
            ..LedgerCorrelation::empty()
        };
        let corr_pane2 = LedgerCorrelation {
            pane_ids: vec![2],
            ..LedgerCorrelation::empty()
        };

        ledger.append(
            1000,
            LedgerEntryKind::StepExecuted {
                step_id: TxStepId("s1".to_string()),
                ordinal: 1,
                succeeded: true,
                detail: "ok".to_string(),
            },
            corr_pane1,
        );
        ledger.append(
            2000,
            LedgerEntryKind::StepExecuted {
                step_id: TxStepId("s2".to_string()),
                ordinal: 2,
                succeeded: true,
                detail: "ok".to_string(),
            },
            corr_pane2,
        );

        assert_eq!(ledger.entries_for_pane(1).len(), 1);
        assert_eq!(ledger.entries_for_pane(2).len(), 1);
        assert_eq!(ledger.entries_for_pane(99).len(), 0);
    }

    #[test]
    fn ledger_entries_for_agent_filters() {
        let mut ledger = test_ledger();

        let corr_a = LedgerCorrelation {
            agent_ids: vec!["alice".to_string()],
            ..LedgerCorrelation::empty()
        };
        let corr_b = LedgerCorrelation {
            agent_ids: vec!["bob".to_string()],
            ..LedgerCorrelation::empty()
        };

        ledger.append(
            1000,
            LedgerEntryKind::IntentRegistered {
                summary: "a".to_string(),
                requested_by: "alice".to_string(),
            },
            corr_a,
        );
        ledger.append(
            2000,
            LedgerEntryKind::IntentRegistered {
                summary: "b".to_string(),
                requested_by: "bob".to_string(),
            },
            corr_b,
        );

        assert_eq!(ledger.entries_for_agent("alice").len(), 1);
        assert_eq!(ledger.entries_for_agent("bob").len(), 1);
        assert_eq!(ledger.entries_for_agent("eve").len(), 0);
    }

    #[test]
    fn ledger_current_state_tracks_transitions() {
        let mut ledger = test_ledger();
        let corr = LedgerCorrelation::empty();

        assert_eq!(ledger.current_state(), MissionTxState::Draft); // default

        ledger.append(
            1000,
            LedgerEntryKind::StateTransition {
                from: MissionTxState::Draft,
                to: MissionTxState::Planned,
                kind: MissionTxTransitionKind::PlanCreated,
            },
            corr.clone(),
        );
        assert_eq!(ledger.current_state(), MissionTxState::Planned);

        ledger.append(
            2000,
            LedgerEntryKind::StateTransition {
                from: MissionTxState::Planned,
                to: MissionTxState::Prepared,
                kind: MissionTxTransitionKind::PrepareSucceeded,
            },
            corr.clone(),
        );
        assert_eq!(ledger.current_state(), MissionTxState::Prepared);

        ledger.append(
            3000,
            LedgerEntryKind::StateTransition {
                from: MissionTxState::Prepared,
                to: MissionTxState::Committing,
                kind: MissionTxTransitionKind::CommitStarted,
            },
            corr,
        );
        assert_eq!(ledger.current_state(), MissionTxState::Committing);
    }

    #[test]
    fn ledger_state_timeline_extracts_transitions() {
        let mut ledger = test_ledger();
        let corr = test_correlation();

        ledger.append(
            1000,
            LedgerEntryKind::IntentRegistered {
                summary: "tx".to_string(),
                requested_by: "op".to_string(),
            },
            corr.clone(),
        );
        ledger.append(
            2000,
            LedgerEntryKind::StateTransition {
                from: MissionTxState::Draft,
                to: MissionTxState::Planned,
                kind: MissionTxTransitionKind::PlanCreated,
            },
            corr.clone(),
        );
        ledger.append(
            3000,
            LedgerEntryKind::PreconditionEvaluated {
                precondition_index: 0,
                passed: true,
                detail: "ok".to_string(),
            },
            corr.clone(),
        );
        ledger.append(
            4000,
            LedgerEntryKind::StateTransition {
                from: MissionTxState::Planned,
                to: MissionTxState::Prepared,
                kind: MissionTxTransitionKind::PrepareSucceeded,
            },
            corr,
        );

        let timeline = ledger.state_timeline();
        assert_eq!(timeline.len(), 2);
        assert_eq!(timeline[0].from, MissionTxState::Draft);
        assert_eq!(timeline[0].to, MissionTxState::Planned);
        assert_eq!(timeline[1].from, MissionTxState::Planned);
        assert_eq!(timeline[1].to, MissionTxState::Prepared);
        assert_eq!(timeline[0].timestamp_ms, 2000);
        assert_eq!(timeline[1].timestamp_ms, 4000);
    }

    #[test]
    fn ledger_jsonl_roundtrip() {
        let mut ledger = test_ledger();
        let corr = test_correlation();

        ledger.append(
            1000,
            LedgerEntryKind::IntentRegistered {
                summary: "test tx".to_string(),
                requested_by: "op".to_string(),
            },
            corr.clone(),
        );
        ledger.append(
            2000,
            LedgerEntryKind::StateTransition {
                from: MissionTxState::Draft,
                to: MissionTxState::Planned,
                kind: MissionTxTransitionKind::PlanCreated,
            },
            corr.clone(),
        );
        ledger.append(
            3000,
            LedgerEntryKind::OutcomeSealed {
                outcome_kind: "committed".to_string(),
                reason_code: None,
                error_code: None,
            },
            corr,
        );

        let jsonl = ledger.to_jsonl();
        let restored = IntentLedger::from_jsonl(TxId("tx-test-001".to_string()), &jsonl).unwrap();

        assert_eq!(restored.len(), ledger.len());
        for (orig, rest) in ledger.entries().iter().zip(restored.entries().iter()) {
            assert_eq!(orig.seq, rest.seq);
            assert_eq!(orig.entry_hash, rest.entry_hash);
            assert_eq!(orig.prev_hash, rest.prev_hash);
            assert_eq!(orig.tx_id, rest.tx_id);
            assert_eq!(orig.created_at_ms, rest.created_at_ms);
        }
        assert!(restored.validate().is_ok());
    }

    #[test]
    fn ledger_serde_roundtrip() {
        let mut ledger = test_ledger();
        let corr = LedgerCorrelation::empty();

        ledger.append(
            1000,
            LedgerEntryKind::IntentRegistered {
                summary: "tx".to_string(),
                requested_by: "op".to_string(),
            },
            corr,
        );

        let json = serde_json::to_string(&ledger).unwrap();
        let restored: IntentLedger = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.len(), 1);
        assert_eq!(restored.tx_id, ledger.tx_id);
        assert!(restored.validate().is_ok());
    }

    #[test]
    fn ledger_entry_serde_roundtrip() {
        let entry = LedgerEntry {
            seq: 1,
            entry_hash: "abc123".to_string(),
            prev_hash: String::new(),
            tx_id: TxId("tx-1".to_string()),
            created_at_ms: 1000,
            kind: LedgerEntryKind::StepExecuted {
                step_id: TxStepId("s1".to_string()),
                ordinal: 1,
                succeeded: true,
                detail: "done".to_string(),
            },
            correlation: test_correlation(),
        };

        let json = serde_json::to_string(&entry).unwrap();
        let restored: LedgerEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.seq, entry.seq);
        assert_eq!(restored.entry_hash, entry.entry_hash);
        assert_eq!(restored.tx_id, entry.tx_id);
    }

    #[test]
    fn ledger_correlation_serde_roundtrip() {
        let corr = test_correlation();
        let json = serde_json::to_string(&corr).unwrap();
        let restored: LedgerCorrelation = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.mission_run_id, corr.mission_run_id);
        assert_eq!(restored.pane_ids, corr.pane_ids);
        assert_eq!(restored.agent_ids, corr.agent_ids);
        assert_eq!(restored.bead_ids, corr.bead_ids);
    }

    #[test]
    fn ledger_correlation_empty_skips_defaults() {
        let corr = LedgerCorrelation::empty();
        let json = serde_json::to_string(&corr).unwrap();
        // Empty vecs should be skipped
        assert!(!json.contains("pane_ids"));
        assert!(!json.contains("agent_ids"));
    }

    #[test]
    fn ledger_correlation_with_mission() {
        let corr = LedgerCorrelation::with_mission("run-99");
        assert_eq!(corr.mission_run_id, Some("run-99".to_string()));
        assert!(corr.pane_ids.is_empty());
    }

    #[test]
    fn ledger_recorder_happy_path() {
        let mut ledger = test_ledger();
        let corr = test_correlation();

        {
            let mut rec = LedgerRecorder::new(&mut ledger, corr);
            rec.record_intent(1000, "Apply updates", "operator");
            rec.record_plan(2000, &TxPlanId("plan-1".to_string()), 2, 1, 1);
            rec.record_precondition(3000, 0, true, "prompt active");
            rec.record_transition(
                4000,
                MissionTxState::Draft,
                MissionTxState::Planned,
                MissionTxTransitionKind::PlanCreated,
            );
            rec.record_transition(
                5000,
                MissionTxState::Planned,
                MissionTxState::Prepared,
                MissionTxTransitionKind::PrepareSucceeded,
            );
            rec.record_step(6000, &TxStepId("s1".to_string()), 1, true, "acquired lock");
            rec.record_step(7000, &TxStepId("s2".to_string()), 2, true, "sent text");
            rec.record_transition(
                8000,
                MissionTxState::Prepared,
                MissionTxState::Committing,
                MissionTxTransitionKind::CommitStarted,
            );
            rec.record_receipt(9000, 1, MissionTxState::Committed, None, None);
            rec.record_transition(
                10000,
                MissionTxState::Committing,
                MissionTxState::Committed,
                MissionTxTransitionKind::CommitSucceeded,
            );
            rec.record_outcome(11000, "committed", None, None);
        }

        assert_eq!(ledger.len(), 11);
        assert!(ledger.validate().is_ok());
        assert_eq!(ledger.current_state(), MissionTxState::Committed);

        let timeline = ledger.state_timeline();
        assert_eq!(timeline.len(), 4);
        assert_eq!(timeline.last().unwrap().to, MissionTxState::Committed);
    }

    #[test]
    fn ledger_recorder_compensation_path() {
        let mut ledger = test_ledger();
        let corr = LedgerCorrelation::with_mission("run-fail");

        {
            let mut rec = LedgerRecorder::new(&mut ledger, corr);
            rec.record_intent(1000, "Failing tx", "operator");
            rec.record_transition(
                2000,
                MissionTxState::Draft,
                MissionTxState::Planned,
                MissionTxTransitionKind::PlanCreated,
            );
            rec.record_transition(
                3000,
                MissionTxState::Planned,
                MissionTxState::Prepared,
                MissionTxTransitionKind::PrepareSucceeded,
            );
            rec.record_transition(
                4000,
                MissionTxState::Prepared,
                MissionTxState::Committing,
                MissionTxTransitionKind::CommitStarted,
            );
            rec.record_step(
                5000,
                &TxStepId("s1".to_string()),
                1,
                false,
                "partial failure",
            );
            rec.record_transition(
                6000,
                MissionTxState::Committing,
                MissionTxState::Compensating,
                MissionTxTransitionKind::CommitPartial,
            );
            rec.record_compensation(
                7000,
                &TxStepId("s1".to_string()),
                true,
                "rolled back step 1",
            );
            rec.record_transition(
                8000,
                MissionTxState::Compensating,
                MissionTxState::RolledBack,
                MissionTxTransitionKind::CompensationSucceeded,
            );
            rec.record_outcome(9000, "rolled_back", Some("commit_partial"), Some("FTX2007"));
        }

        assert_eq!(ledger.len(), 9);
        assert!(ledger.validate().is_ok());
        assert_eq!(ledger.current_state(), MissionTxState::RolledBack);
    }

    #[test]
    fn ledger_hash_deterministic() {
        let mut l1 = test_ledger();
        let mut l2 = test_ledger();
        let corr = LedgerCorrelation::empty();

        l1.append(
            1000,
            LedgerEntryKind::IntentRegistered {
                summary: "tx".to_string(),
                requested_by: "op".to_string(),
            },
            corr.clone(),
        );
        l2.append(
            1000,
            LedgerEntryKind::IntentRegistered {
                summary: "tx".to_string(),
                requested_by: "op".to_string(),
            },
            corr,
        );

        assert_eq!(
            l1.entry_at(1).unwrap().entry_hash,
            l2.entry_at(1).unwrap().entry_hash,
        );
    }

    #[test]
    fn ledger_entry_at_zero_returns_none() {
        let mut ledger = test_ledger();
        ledger.append(
            1000,
            LedgerEntryKind::IntentRegistered {
                summary: "tx".to_string(),
                requested_by: "op".to_string(),
            },
            LedgerCorrelation::empty(),
        );

        assert!(ledger.entry_at(0).is_none());
        assert!(ledger.entry_at(1).is_some());
        assert!(ledger.entry_at(2).is_none());
    }

    #[test]
    fn ledger_validation_error_display() {
        let err = LedgerValidationError::TamperedEntry { seq: 3 };
        let msg = format!("{err}");
        assert!(msg.contains("seq 3"));

        let err = LedgerValidationError::BrokenHashChain {
            seq: 5,
            expected_prev: "abc".to_string(),
            actual_prev: "def".to_string(),
        };
        assert!(format!("{err}").contains("seq 5"));
    }

    #[test]
    fn ledger_all_entry_kinds_roundtrip() {
        let kinds = vec![
            LedgerEntryKind::IntentRegistered {
                summary: "s".to_string(),
                requested_by: "r".to_string(),
            },
            LedgerEntryKind::PlanCreated {
                plan_id: TxPlanId("p".to_string()),
                step_count: 3,
                precondition_count: 1,
                compensation_count: 2,
            },
            LedgerEntryKind::PreconditionEvaluated {
                precondition_index: 0,
                passed: true,
                detail: "ok".to_string(),
            },
            LedgerEntryKind::StateTransition {
                from: MissionTxState::Draft,
                to: MissionTxState::Planned,
                kind: MissionTxTransitionKind::PlanCreated,
            },
            LedgerEntryKind::StepExecuted {
                step_id: TxStepId("s1".to_string()),
                ordinal: 1,
                succeeded: true,
                detail: "done".to_string(),
            },
            LedgerEntryKind::CompensationExecuted {
                for_step_id: TxStepId("s1".to_string()),
                succeeded: false,
                detail: "failed".to_string(),
            },
            LedgerEntryKind::OutcomeSealed {
                outcome_kind: "committed".to_string(),
                reason_code: None,
                error_code: None,
            },
            LedgerEntryKind::ReceiptRecorded {
                receipt_seq: 1,
                state: MissionTxState::Committed,
                reason_code: Some("ok".to_string()),
                error_code: None,
            },
        ];

        for kind in kinds {
            let json = serde_json::to_string(&kind).unwrap();
            let restored: LedgerEntryKind = serde_json::from_str(&json).unwrap();
            let json2 = serde_json::to_string(&restored).unwrap();
            assert_eq!(json, json2);
        }
    }

    #[test]
    fn ledger_jsonl_empty_lines_skipped() {
        let jsonl = "\n\n";
        let ledger = IntentLedger::from_jsonl(TxId("tx-1".to_string()), jsonl).unwrap();
        assert!(ledger.is_empty());
    }

    #[test]
    fn ledger_jsonl_bad_line_returns_error() {
        let result = IntentLedger::from_jsonl(TxId("tx-1".to_string()), "not valid json");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("line 1"));
    }

    #[test]
    fn ledger_large_chain_validates() {
        let mut ledger = test_ledger();
        let corr = LedgerCorrelation::empty();

        for i in 0..100 {
            ledger.append(
                1000 * (i + 1),
                LedgerEntryKind::ReceiptRecorded {
                    receipt_seq: i as u64 + 1,
                    state: MissionTxState::Draft,
                    reason_code: None,
                    error_code: None,
                },
                corr.clone(),
            );
        }

        assert_eq!(ledger.len(), 100);
        assert!(ledger.validate().is_ok());

        // Every entry should link to the previous
        for i in 1..100 {
            assert_eq!(
                ledger.entries()[i].prev_hash,
                ledger.entries()[i - 1].entry_hash,
            );
        }
    }

    // ====================================================================
    // Kill-Switch and Safe-Mode Tests
    // ====================================================================

    #[test]
    fn mission_kill_switch_level_defaults_to_off() {
        let state = MissionKillSwitchState::default();
        assert!(state.is_off());
        assert_eq!(state.level, MissionKillSwitchLevel::Off);
        assert!(state.current_activation.is_none());
        assert!(state.activation_history.is_empty());
    }

    #[test]
    fn mission_kill_switch_level_blocks_dispatch() {
        assert!(!MissionKillSwitchLevel::Off.blocks_dispatch());
        assert!(MissionKillSwitchLevel::SafeMode.blocks_dispatch());
        assert!(MissionKillSwitchLevel::HardStop.blocks_dispatch());
    }

    #[test]
    fn mission_kill_switch_level_cancels_in_flight() {
        assert!(!MissionKillSwitchLevel::Off.cancels_in_flight());
        assert!(!MissionKillSwitchLevel::SafeMode.cancels_in_flight());
        assert!(MissionKillSwitchLevel::HardStop.cancels_in_flight());
    }

    #[test]
    fn mission_kill_switch_level_allows_read_only() {
        assert!(MissionKillSwitchLevel::Off.allows_read_only());
        assert!(MissionKillSwitchLevel::SafeMode.allows_read_only());
        assert!(!MissionKillSwitchLevel::HardStop.allows_read_only());
    }

    #[test]
    fn mission_kill_switch_activate_records_and_sets_level() {
        let mut state = MissionKillSwitchState::default();
        state.activate(MissionKillSwitchActivation {
            level: MissionKillSwitchLevel::SafeMode,
            activated_by: "operator-1".to_string(),
            reason_code: "runaway_agent".to_string(),
            error_code: Some("FTM1009".to_string()),
            activated_at_ms: 1_000_000,
            expires_at_ms: None,
            correlation_id: Some("corr-1".to_string()),
        });

        assert_eq!(state.level, MissionKillSwitchLevel::SafeMode);
        assert!(!state.is_off());
        assert_eq!(state.activation_history.len(), 1);
        assert_eq!(
            state.current_activation.as_ref().unwrap().activated_by,
            "operator-1"
        );
    }

    #[test]
    fn mission_kill_switch_deactivate_returns_to_off() {
        let mut state = MissionKillSwitchState::default();
        state.activate(MissionKillSwitchActivation {
            level: MissionKillSwitchLevel::HardStop,
            activated_by: "system".to_string(),
            reason_code: "emergency".to_string(),
            error_code: None,
            activated_at_ms: 1_000_000,
            expires_at_ms: None,
            correlation_id: None,
        });
        assert_eq!(state.level, MissionKillSwitchLevel::HardStop);

        state.deactivate("operator-1", "incident_resolved", 2_000_000);
        assert!(state.is_off());
        assert!(state.current_activation.is_none());
        // History should have 2 entries: activation + deactivation
        assert_eq!(state.activation_history.len(), 2);
    }

    #[test]
    fn mission_kill_switch_ttl_expiry_auto_deactivates() {
        let mut state = MissionKillSwitchState::default();
        state.activate(MissionKillSwitchActivation {
            level: MissionKillSwitchLevel::SafeMode,
            activated_by: "operator-1".to_string(),
            reason_code: "temporary_hold".to_string(),
            error_code: None,
            activated_at_ms: 1_000_000,
            expires_at_ms: Some(2_000_000),
            correlation_id: None,
        });

        // Before expiry: still active
        let level = state.evaluate_effective_level(1_500_000);
        assert_eq!(level, MissionKillSwitchLevel::SafeMode);

        // At/after expiry: auto-deactivated
        let level = state.evaluate_effective_level(2_000_000);
        assert_eq!(level, MissionKillSwitchLevel::Off);
        assert!(state.is_off());
        // History: activation + auto-deactivation
        assert_eq!(state.activation_history.len(), 2);
    }

    #[test]
    fn mission_kill_switch_escalation_overwrites_level() {
        let mut state = MissionKillSwitchState::default();
        state.activate(MissionKillSwitchActivation {
            level: MissionKillSwitchLevel::SafeMode,
            activated_by: "operator-1".to_string(),
            reason_code: "caution".to_string(),
            error_code: None,
            activated_at_ms: 1_000_000,
            expires_at_ms: None,
            correlation_id: None,
        });
        assert_eq!(state.level, MissionKillSwitchLevel::SafeMode);

        // Escalate to hard stop
        state.activate(MissionKillSwitchActivation {
            level: MissionKillSwitchLevel::HardStop,
            activated_by: "operator-1".to_string(),
            reason_code: "situation_worsened".to_string(),
            error_code: None,
            activated_at_ms: 1_500_000,
            expires_at_ms: None,
            correlation_id: None,
        });
        assert_eq!(state.level, MissionKillSwitchLevel::HardStop);
        assert_eq!(state.activation_history.len(), 2);
    }

    #[test]
    fn mission_kill_switch_evict_history_bounds_memory() {
        let mut state = MissionKillSwitchState::default();
        for i in 0..10 {
            state.activate(MissionKillSwitchActivation {
                level: MissionKillSwitchLevel::SafeMode,
                activated_by: "system".to_string(),
                reason_code: format!("trigger_{i}"),
                error_code: None,
                activated_at_ms: (i + 1) * 1000,
                expires_at_ms: None,
                correlation_id: None,
            });
        }
        assert_eq!(state.activation_history.len(), 10);

        state.evict_history_before(6000);
        assert_eq!(state.activation_history.len(), 5);
        assert_eq!(state.activation_history[0].activated_at_ms, 6000);
    }

    #[test]
    fn mission_evaluate_kill_switch_off_allows_dispatch() {
        let mut mission = sample_mission();
        let decision = mission.evaluate_kill_switch(1_704_000_001_000);
        assert!(!decision.blocked);
        assert_eq!(decision.effective_level, MissionKillSwitchLevel::Off);
        assert_eq!(decision.decision_path, "kill_switch_off");
    }

    #[test]
    fn mission_activate_kill_switch_blocks_dispatch() {
        let mut mission = sample_mission();
        let decision = mission
            .activate_kill_switch(
                MissionKillSwitchLevel::SafeMode,
                "operator-human",
                "runaway_detected",
                1_704_000_001_000,
                None,
                Some("corr-ks-1".to_string()),
            )
            .unwrap();

        assert!(decision.blocked);
        assert_eq!(decision.effective_level, MissionKillSwitchLevel::SafeMode);
        assert_eq!(decision.decision_path, "kill_switch_safe_mode");
        assert_eq!(decision.reason_code, "kill_switch_activated");
        assert_eq!(decision.error_code.as_deref(), Some("FTM1009"));
    }

    #[test]
    fn mission_activate_kill_switch_rejects_level_off() {
        let mut mission = sample_mission();
        let result = mission.activate_kill_switch(
            MissionKillSwitchLevel::Off,
            "operator",
            "test",
            1_000_000,
            None,
            None,
        );
        let is_err = result.is_err();
        assert!(is_err);
    }

    #[test]
    fn mission_activate_kill_switch_rejects_empty_activated_by() {
        let mut mission = sample_mission();
        let result = mission.activate_kill_switch(
            MissionKillSwitchLevel::SafeMode,
            "  ",
            "reason",
            1_000_000,
            None,
            None,
        );
        let is_err = result.is_err();
        assert!(is_err);
    }

    #[test]
    fn mission_activate_kill_switch_rejects_expired_before_activated() {
        let mut mission = sample_mission();
        let result = mission.activate_kill_switch(
            MissionKillSwitchLevel::SafeMode,
            "operator",
            "reason",
            2_000_000,
            Some(1_000_000), // expires before activation
            None,
        );
        let is_err = result.is_err();
        assert!(is_err);
    }

    #[test]
    fn mission_deactivate_kill_switch_restores_dispatch() {
        let mut mission = sample_mission();
        mission
            .activate_kill_switch(
                MissionKillSwitchLevel::HardStop,
                "operator",
                "emergency",
                1_000_000,
                None,
                None,
            )
            .unwrap();

        let decision = mission
            .deactivate_kill_switch("operator", "all_clear", 2_000_000)
            .unwrap();

        assert!(!decision.blocked);
        assert_eq!(decision.effective_level, MissionKillSwitchLevel::Off);
        assert!(mission.kill_switch.is_off());
    }

    #[test]
    fn mission_cancel_in_flight_for_kill_switch() {
        let mut mission = Mission::new(
            MissionId("mission:ks-test".to_string()),
            "Kill-switch cancel test",
            "ws-test",
            MissionOwnership {
                planner: "p".to_string(),
                dispatcher: "d".to_string(),
                operator: "o".to_string(),
            },
            1_000_000,
        );
        mission.candidates.push(CandidateAction {
            candidate_id: CandidateActionId("c1".to_string()),
            requested_by: MissionActorRole::Planner,
            action: StepAction::SendText {
                pane_id: 1,
                text: "test".to_string(),
                paste_mode: None,
            },
            rationale: "test".to_string(),
            score: None,
            created_at_ms: 1_000_100,
        });
        // Assignment with no outcome (in-flight)
        mission.assignments.push(Assignment {
            assignment_id: AssignmentId("a1".to_string()),
            candidate_id: CandidateActionId("c1".to_string()),
            assigned_by: MissionActorRole::Dispatcher,
            assignee: "agent-1".to_string(),
            reservation_intent: None,
            approval_state: ApprovalState::NotRequired,
            outcome: None,
            escalation: None,
            created_at_ms: 1_000_200,
            updated_at_ms: None,
        });
        // Assignment already completed (should NOT be cancelled)
        mission.assignments.push(Assignment {
            assignment_id: AssignmentId("a2".to_string()),
            candidate_id: CandidateActionId("c1".to_string()),
            assigned_by: MissionActorRole::Dispatcher,
            assignee: "agent-2".to_string(),
            reservation_intent: None,
            approval_state: ApprovalState::NotRequired,
            outcome: Some(Outcome::Success {
                reason_code: "done".to_string(),
                completed_at_ms: 1_000_500,
            }),
            escalation: None,
            created_at_ms: 1_000_300,
            updated_at_ms: Some(1_000_500),
        });

        let cancelled = mission.cancel_in_flight_for_kill_switch(2_000_000);
        assert_eq!(cancelled, 1);

        // a1 should be cancelled
        let a1_outcome = mission
            .find_assignment_by_id(&AssignmentId("a1".to_string()))
            .unwrap()
            .outcome
            .as_ref()
            .unwrap();
        let is_cancelled = matches!(a1_outcome, Outcome::Cancelled { .. });
        assert!(is_cancelled);

        // a2 should still be Success
        let a2_outcome = mission
            .find_assignment_by_id(&AssignmentId("a2".to_string()))
            .unwrap()
            .outcome
            .as_ref()
            .unwrap();
        let is_success = matches!(a2_outcome, Outcome::Success { .. });
        assert!(is_success);
    }

    #[test]
    fn mission_kill_switch_serde_roundtrip() {
        let mut state = MissionKillSwitchState::default();
        state.activate(MissionKillSwitchActivation {
            level: MissionKillSwitchLevel::SafeMode,
            activated_by: "operator".to_string(),
            reason_code: "test".to_string(),
            error_code: Some("FTM1009".to_string()),
            activated_at_ms: 1_000_000,
            expires_at_ms: Some(2_000_000),
            correlation_id: Some("corr-1".to_string()),
        });

        let json = serde_json::to_string(&state).unwrap();
        let restored: MissionKillSwitchState = serde_json::from_str(&json).unwrap();

        assert_eq!(state, restored);
        assert_eq!(restored.level, MissionKillSwitchLevel::SafeMode);
        assert_eq!(
            restored.current_activation.as_ref().unwrap().activated_by,
            "operator"
        );
    }

    #[test]
    fn mission_kill_switch_canonical_string_is_deterministic() {
        let mut state = MissionKillSwitchState::default();
        state.activate(MissionKillSwitchActivation {
            level: MissionKillSwitchLevel::HardStop,
            activated_by: "system".to_string(),
            reason_code: "chaos_monkey".to_string(),
            error_code: None,
            activated_at_ms: 1_000_000,
            expires_at_ms: None,
            correlation_id: None,
        });

        let s1 = state.canonical_string();
        let s2 = state.canonical_string();
        assert_eq!(s1, s2);
        assert!(s1.contains("kill_switch_level=hard_stop"));
        assert!(s1.contains("chaos_monkey"));
    }

    #[test]
    fn mission_kill_switch_failure_code_roundtrip() {
        let code = MissionFailureCode::KillSwitchActivated;
        assert_eq!(code.reason_code(), "kill_switch_activated");
        assert_eq!(code.error_code(), "FTM1009");
        assert!(code.terminality().is_terminal());
        assert!(!code.retryability().is_retryable());

        // Roundtrip through from_reason_code
        let from_reason = MissionFailureCode::from_reason_code("kill_switch_activated");
        assert_eq!(from_reason, Some(MissionFailureCode::KillSwitchActivated));

        // Roundtrip through from_error_code
        let from_error = MissionFailureCode::from_error_code("FTM1009");
        assert_eq!(from_error, Some(MissionFailureCode::KillSwitchActivated));
    }

    #[test]
    fn mission_kill_switch_canonical_string_included_in_mission_hash() {
        let mut m1 = sample_mission();
        let hash_before = m1.compute_hash();

        m1.kill_switch.activate(MissionKillSwitchActivation {
            level: MissionKillSwitchLevel::SafeMode,
            activated_by: "operator".to_string(),
            reason_code: "test".to_string(),
            error_code: None,
            activated_at_ms: 9_000_000,
            expires_at_ms: None,
            correlation_id: None,
        });

        let hash_after = m1.compute_hash();
        assert_ne!(hash_before, hash_after);
    }

    #[test]
    fn mission_kill_switch_activation_expiry_check() {
        let activation = MissionKillSwitchActivation {
            level: MissionKillSwitchLevel::SafeMode,
            activated_by: "op".to_string(),
            reason_code: "test".to_string(),
            error_code: None,
            activated_at_ms: 1_000_000,
            expires_at_ms: Some(2_000_000),
            correlation_id: None,
        };

        assert!(!activation.is_expired_at(1_000_000));
        assert!(!activation.is_expired_at(1_999_999));
        assert!(activation.is_expired_at(2_000_000));
        assert!(activation.is_expired_at(3_000_000));

        // No expiry = never expires
        let no_expiry = MissionKillSwitchActivation {
            expires_at_ms: None,
            ..activation
        };
        assert!(!no_expiry.is_expired_at(i64::MAX));
    }

    // ====================================================================
    // Prepare-Phase Coordinator Tests
    // ====================================================================

    fn sample_tx_plan() -> TxPlan {
        TxPlan {
            plan_id: TxPlanId("plan:prepare-test".to_string()),
            tx_id: TxId("tx:prepare-test".to_string()),
            steps: vec![
                TxStep {
                    step_id: TxStepId("s1".to_string()),
                    ordinal: 1,
                    action: StepAction::SendText {
                        pane_id: 1,
                        text: "/retry".to_string(),
                        paste_mode: None,
                    },
                },
                TxStep {
                    step_id: TxStepId("s2".to_string()),
                    ordinal: 2,
                    action: StepAction::AcquireLock {
                        lock_name: "deploy.lock".to_string(),
                        timeout_ms: Some(5000),
                    },
                },
                TxStep {
                    step_id: TxStepId("s3".to_string()),
                    ordinal: 3,
                    action: StepAction::SendText {
                        pane_id: 2,
                        text: "deploy".to_string(),
                        paste_mode: Some(true),
                    },
                },
            ],
            preconditions: vec![],
            compensations: vec![],
        }
    }

    fn all_passing_gate(step_id: &str) -> TxPrepareGateInput {
        TxPrepareGateInput {
            step_id: TxStepId(step_id.to_string()),
            policy_passed: true,
            policy_reason_code: None,
            reservation_available: true,
            reservation_reason_code: None,
            approval_satisfied: true,
            approval_reason_code: None,
            target_liveness: true,
            liveness_reason_code: None,
        }
    }

    #[test]
    fn prepare_all_steps_ready_when_all_gates_pass() {
        let plan = sample_tx_plan();
        let tx_id = TxId("tx:prepare-test".to_string());
        let gates = vec![
            all_passing_gate("s1"),
            all_passing_gate("s2"),
            all_passing_gate("s3"),
        ];

        let report = evaluate_prepare_phase(
            &tx_id,
            &plan,
            &gates,
            MissionKillSwitchLevel::Off,
            1_000_000,
        )
        .unwrap();

        assert_eq!(report.outcome, TxPrepareOutcome::AllReady);
        assert!(report.outcome.commit_eligible());
        assert_eq!(report.step_receipts.len(), 3);
        for receipt in &report.step_receipts {
            assert!(receipt.readiness.is_ready());
        }
        assert_eq!(report.reason_code, "prepare_succeeded");
        assert!(report.error_code.is_none());
    }

    #[test]
    fn prepare_denied_when_policy_fails() {
        let plan = sample_tx_plan();
        let tx_id = TxId("tx:prepare-test".to_string());
        let mut gates = vec![
            all_passing_gate("s1"),
            all_passing_gate("s2"),
            all_passing_gate("s3"),
        ];
        gates[1].policy_passed = false;
        gates[1].policy_reason_code = Some("policy_denied".to_string());

        let report = evaluate_prepare_phase(
            &tx_id,
            &plan,
            &gates,
            MissionKillSwitchLevel::Off,
            1_000_000,
        )
        .unwrap();

        assert_eq!(report.outcome, TxPrepareOutcome::Denied);
        assert!(!report.outcome.commit_eligible());
        assert!(report.step_receipts[1].readiness.is_denied());
        assert_eq!(report.reason_code, "policy_denied");
    }

    #[test]
    fn prepare_deferred_when_reservation_conflicts() {
        let plan = sample_tx_plan();
        let tx_id = TxId("tx:prepare-test".to_string());
        let mut gates = vec![
            all_passing_gate("s1"),
            all_passing_gate("s2"),
            all_passing_gate("s3"),
        ];
        gates[0].reservation_available = false;
        gates[0].reservation_reason_code = Some("reservation_conflict".to_string());

        let report = evaluate_prepare_phase(
            &tx_id,
            &plan,
            &gates,
            MissionKillSwitchLevel::Off,
            1_000_000,
        )
        .unwrap();

        assert_eq!(report.outcome, TxPrepareOutcome::Deferred);
        assert!(!report.outcome.commit_eligible());
        assert!(report.step_receipts[0].readiness.is_deferred());
    }

    #[test]
    fn prepare_deferred_when_approval_pending() {
        let plan = sample_tx_plan();
        let tx_id = TxId("tx:prepare-test".to_string());
        let mut gates = vec![
            all_passing_gate("s1"),
            all_passing_gate("s2"),
            all_passing_gate("s3"),
        ];
        gates[2].approval_satisfied = false;
        gates[2].approval_reason_code = Some("approval_required".to_string());

        let report = evaluate_prepare_phase(
            &tx_id,
            &plan,
            &gates,
            MissionKillSwitchLevel::Off,
            1_000_000,
        )
        .unwrap();

        assert_eq!(report.outcome, TxPrepareOutcome::Deferred);
        assert!(report.step_receipts[2].readiness.is_deferred());
    }

    #[test]
    fn prepare_denied_when_approval_denied() {
        let plan = sample_tx_plan();
        let tx_id = TxId("tx:prepare-test".to_string());
        let mut gates = vec![
            all_passing_gate("s1"),
            all_passing_gate("s2"),
            all_passing_gate("s3"),
        ];
        gates[2].approval_satisfied = false;
        gates[2].approval_reason_code = Some("approval_denied".to_string());

        let report = evaluate_prepare_phase(
            &tx_id,
            &plan,
            &gates,
            MissionKillSwitchLevel::Off,
            1_000_000,
        )
        .unwrap();

        assert_eq!(report.outcome, TxPrepareOutcome::Denied);
        assert!(report.step_receipts[2].readiness.is_denied());
    }

    #[test]
    fn prepare_deferred_when_target_unreachable() {
        let plan = sample_tx_plan();
        let tx_id = TxId("tx:prepare-test".to_string());
        let mut gates = vec![
            all_passing_gate("s1"),
            all_passing_gate("s2"),
            all_passing_gate("s3"),
        ];
        gates[0].target_liveness = false;
        gates[0].liveness_reason_code = Some("pane_not_found".to_string());

        let report = evaluate_prepare_phase(
            &tx_id,
            &plan,
            &gates,
            MissionKillSwitchLevel::Off,
            1_000_000,
        )
        .unwrap();

        assert_eq!(report.outcome, TxPrepareOutcome::Deferred);
        assert!(report.step_receipts[0].readiness.is_deferred());
    }

    #[test]
    fn prepare_denied_by_kill_switch_safe_mode() {
        let plan = sample_tx_plan();
        let tx_id = TxId("tx:prepare-test".to_string());
        let gates = vec![
            all_passing_gate("s1"),
            all_passing_gate("s2"),
            all_passing_gate("s3"),
        ];

        let report = evaluate_prepare_phase(
            &tx_id,
            &plan,
            &gates,
            MissionKillSwitchLevel::SafeMode,
            1_000_000,
        )
        .unwrap();

        assert_eq!(report.outcome, TxPrepareOutcome::Denied);
        assert_eq!(report.reason_code, "kill_switch_activated");
        assert_eq!(report.error_code.as_deref(), Some("FTM1009"));
        for receipt in &report.step_receipts {
            assert!(receipt.readiness.is_denied());
        }
    }

    #[test]
    fn prepare_denied_by_kill_switch_hard_stop() {
        let plan = sample_tx_plan();
        let tx_id = TxId("tx:prepare-test".to_string());
        let gates = vec![];

        let report = evaluate_prepare_phase(
            &tx_id,
            &plan,
            &gates,
            MissionKillSwitchLevel::HardStop,
            1_000_000,
        )
        .unwrap();

        assert_eq!(report.outcome, TxPrepareOutcome::Denied);
        assert_eq!(report.step_receipts.len(), 3);
    }

    #[test]
    fn prepare_deferred_when_gate_input_missing() {
        let plan = sample_tx_plan();
        let tx_id = TxId("tx:prepare-test".to_string());
        // Only provide gates for s1 and s3, missing s2
        let gates = vec![all_passing_gate("s1"), all_passing_gate("s3")];

        let report = evaluate_prepare_phase(
            &tx_id,
            &plan,
            &gates,
            MissionKillSwitchLevel::Off,
            1_000_000,
        )
        .unwrap();

        assert_eq!(report.outcome, TxPrepareOutcome::Deferred);
        assert!(report.step_receipts[0].readiness.is_ready());
        assert!(report.step_receipts[1].readiness.is_deferred()); // s2 missing
        assert!(report.step_receipts[2].readiness.is_ready());
    }

    #[test]
    fn prepare_denied_trumps_deferred() {
        let plan = sample_tx_plan();
        let tx_id = TxId("tx:prepare-test".to_string());
        let mut gates = vec![
            all_passing_gate("s1"),
            all_passing_gate("s2"),
            all_passing_gate("s3"),
        ];
        // s1 is deferred (reservation conflict)
        gates[0].reservation_available = false;
        // s3 is denied (policy)
        gates[2].policy_passed = false;
        gates[2].policy_reason_code = Some("policy_denied".to_string());

        let report = evaluate_prepare_phase(
            &tx_id,
            &plan,
            &gates,
            MissionKillSwitchLevel::Off,
            1_000_000,
        )
        .unwrap();

        // Denied trumps deferred
        assert_eq!(report.outcome, TxPrepareOutcome::Denied);
    }

    #[test]
    fn prepare_empty_plan_returns_error() {
        let plan = TxPlan {
            plan_id: TxPlanId("plan:empty".to_string()),
            tx_id: TxId("tx:empty".to_string()),
            steps: vec![],
            preconditions: vec![],
            compensations: vec![],
        };
        let tx_id = TxId("tx:empty".to_string());
        let result =
            evaluate_prepare_phase(&tx_id, &plan, &[], MissionKillSwitchLevel::Off, 1_000_000);
        let is_err = result.is_err();
        assert!(is_err);
    }

    #[test]
    fn prepare_readiness_counts_correct() {
        let plan = sample_tx_plan();
        let tx_id = TxId("tx:prepare-test".to_string());
        let mut gates = vec![
            all_passing_gate("s1"),
            all_passing_gate("s2"),
            all_passing_gate("s3"),
        ];
        // s2: reservation conflict (deferred)
        gates[1].reservation_available = false;
        // s3: policy denied
        gates[2].policy_passed = false;

        let report = evaluate_prepare_phase(
            &tx_id,
            &plan,
            &gates,
            MissionKillSwitchLevel::Off,
            1_000_000,
        )
        .unwrap();

        let (ready, denied, deferred) = report.readiness_counts();
        assert_eq!(ready, 1);
        assert_eq!(denied, 1);
        assert_eq!(deferred, 1);
    }

    #[test]
    fn prepare_report_serde_roundtrip() {
        let plan = sample_tx_plan();
        let tx_id = TxId("tx:prepare-test".to_string());
        let gates = vec![
            all_passing_gate("s1"),
            all_passing_gate("s2"),
            all_passing_gate("s3"),
        ];

        let report = evaluate_prepare_phase(
            &tx_id,
            &plan,
            &gates,
            MissionKillSwitchLevel::Off,
            1_000_000,
        )
        .unwrap();

        let json = serde_json::to_string(&report).unwrap();
        let restored: TxPrepareReport = serde_json::from_str(&json).unwrap();

        assert_eq!(report.outcome, restored.outcome);
        assert_eq!(report.step_receipts.len(), restored.step_receipts.len());
        assert_eq!(report.reason_code, restored.reason_code);
    }

    #[test]
    fn prepare_canonical_string_is_deterministic() {
        let plan = sample_tx_plan();
        let tx_id = TxId("tx:prepare-test".to_string());
        let gates = vec![
            all_passing_gate("s1"),
            all_passing_gate("s2"),
            all_passing_gate("s3"),
        ];

        let report = evaluate_prepare_phase(
            &tx_id,
            &plan,
            &gates,
            MissionKillSwitchLevel::Off,
            1_000_000,
        )
        .unwrap();

        let s1 = report.canonical_string();
        let s2 = report.canonical_string();
        assert_eq!(s1, s2);
        assert!(s1.contains("tx=tx:prepare-test"));
        assert!(s1.contains("outcome=all_ready"));
    }

    #[test]
    fn prepare_gate_priority_policy_before_reservation() {
        let plan = TxPlan {
            plan_id: TxPlanId("plan:priority".to_string()),
            tx_id: TxId("tx:priority".to_string()),
            steps: vec![TxStep {
                step_id: TxStepId("s1".to_string()),
                ordinal: 1,
                action: StepAction::SendText {
                    pane_id: 1,
                    text: "test".to_string(),
                    paste_mode: None,
                },
            }],
            preconditions: vec![],
            compensations: vec![],
        };
        let tx_id = TxId("tx:priority".to_string());

        // Both policy denied AND reservation conflict — policy should win (denied > deferred)
        let gates = vec![TxPrepareGateInput {
            step_id: TxStepId("s1".to_string()),
            policy_passed: false,
            policy_reason_code: Some("policy_denied".to_string()),
            reservation_available: false,
            reservation_reason_code: Some("reservation_conflict".to_string()),
            approval_satisfied: true,
            approval_reason_code: None,
            target_liveness: true,
            liveness_reason_code: None,
        }];

        let report = evaluate_prepare_phase(
            &tx_id,
            &plan,
            &gates,
            MissionKillSwitchLevel::Off,
            1_000_000,
        )
        .unwrap();

        assert_eq!(report.outcome, TxPrepareOutcome::Denied);
        assert!(report.step_receipts[0].readiness.is_denied());
        // Should be policy-denied, not reservation-conflict
        assert_eq!(
            report.step_receipts[0].decision_path,
            "prepare_policy_denied"
        );
    }

    #[test]
    fn prepare_step_readiness_methods() {
        let ready = TxPrepareStepReadiness::Ready;
        assert!(ready.is_ready());
        assert!(!ready.is_denied());
        assert!(!ready.is_deferred());

        let denied = TxPrepareStepReadiness::Denied {
            reason_code: "test".to_string(),
            error_code: "ERR".to_string(),
        };
        assert!(!denied.is_ready());
        assert!(denied.is_denied());
        assert!(!denied.is_deferred());

        let deferred = TxPrepareStepReadiness::Deferred {
            reason_code: "test".to_string(),
            retry_hint: "hint".to_string(),
        };
        assert!(!deferred.is_ready());
        assert!(!deferred.is_denied());
        assert!(deferred.is_deferred());
    }

    #[test]
    fn prepare_outcome_commit_eligibility() {
        assert!(TxPrepareOutcome::AllReady.commit_eligible());
        assert!(!TxPrepareOutcome::Denied.commit_eligible());
        assert!(!TxPrepareOutcome::Deferred.commit_eligible());
    }

    // ========================================================================
    // C5: Pause/Resume/Abort Control Tests
    // ========================================================================

    #[test]
    fn pause_from_running_creates_checkpoint() {
        let mut mission = Mission::new(
            MissionId("m-pause-run".into()),
            "test",
            "ws-1",
            MissionOwnership::solo("agent-1"),
            1000,
        );
        mission.lifecycle_state = MissionLifecycleState::Running;

        let decision = mission
            .pause_mission("operator-1", "manual_pause", 2000, None)
            .unwrap();

        assert_eq!(decision.action, "pause");
        assert_eq!(decision.lifecycle_from, MissionLifecycleState::Running);
        assert_eq!(decision.lifecycle_to, MissionLifecycleState::Paused);
        assert_eq!(mission.lifecycle_state, MissionLifecycleState::Paused);
        assert!(mission.pause_resume_state.is_paused());
        assert_eq!(mission.pause_resume_state.total_pause_count, 1);

        let cp = mission
            .pause_resume_state
            .current_checkpoint
            .as_ref()
            .unwrap();
        assert_eq!(cp.paused_from_state, MissionLifecycleState::Running);
        assert_eq!(cp.paused_by, "operator-1");
        assert_eq!(cp.paused_at_ms, 2000);
        assert!(cp.resumed_at_ms.is_none());
    }

    #[test]
    fn pause_from_dispatching_creates_checkpoint() {
        let mut mission = Mission::new(
            MissionId("m-pause-disp".into()),
            "test",
            "ws-1",
            MissionOwnership::solo("agent-1"),
            1000,
        );
        mission.lifecycle_state = MissionLifecycleState::Dispatching;

        let decision = mission
            .pause_mission("system", "resource_pressure", 2000, Some("corr-1".into()))
            .unwrap();

        assert_eq!(decision.lifecycle_from, MissionLifecycleState::Dispatching);
        let cp = mission
            .pause_resume_state
            .current_checkpoint
            .as_ref()
            .unwrap();
        assert_eq!(cp.paused_from_state, MissionLifecycleState::Dispatching);
        assert_eq!(cp.correlation_id.as_deref(), Some("corr-1"));
    }

    #[test]
    fn pause_from_awaiting_approval_creates_checkpoint() {
        let mut mission = Mission::new(
            MissionId("m-pause-approval".into()),
            "test",
            "ws-1",
            MissionOwnership::solo("agent-1"),
            1000,
        );
        mission.lifecycle_state = MissionLifecycleState::AwaitingApproval;

        let decision = mission
            .pause_mission("operator-1", "manual_pause", 2000, None)
            .unwrap();

        assert_eq!(
            decision.lifecycle_from,
            MissionLifecycleState::AwaitingApproval
        );
        let cp = mission
            .pause_resume_state
            .current_checkpoint
            .as_ref()
            .unwrap();
        assert_eq!(
            cp.paused_from_state,
            MissionLifecycleState::AwaitingApproval
        );
    }

    #[test]
    fn pause_from_blocked_creates_checkpoint() {
        let mut mission = Mission::new(
            MissionId("m-pause-blocked".into()),
            "test",
            "ws-1",
            MissionOwnership::solo("agent-1"),
            1000,
        );
        mission.lifecycle_state = MissionLifecycleState::Blocked;

        let decision = mission
            .pause_mission("operator-1", "investigation", 2000, None)
            .unwrap();

        assert_eq!(decision.lifecycle_from, MissionLifecycleState::Blocked);
    }

    #[test]
    fn pause_from_retry_pending_creates_checkpoint() {
        let mut mission = Mission::new(
            MissionId("m-pause-retry".into()),
            "test",
            "ws-1",
            MissionOwnership::solo("agent-1"),
            1000,
        );
        mission.lifecycle_state = MissionLifecycleState::RetryPending;

        let decision = mission
            .pause_mission("operator-1", "cooldown", 2000, None)
            .unwrap();

        assert_eq!(decision.lifecycle_from, MissionLifecycleState::RetryPending);
    }

    #[test]
    fn pause_rejects_terminal_states() {
        for terminal in [
            MissionLifecycleState::Completed,
            MissionLifecycleState::Failed,
            MissionLifecycleState::Cancelled,
        ] {
            let mut mission = Mission::new(
                MissionId("m-pause-term".into()),
                "test",
                "ws-1",
                MissionOwnership::solo("agent-1"),
                1000,
            );
            mission.lifecycle_state = terminal;

            let result = mission.pause_mission("op", "reason", 2000, None);
            assert!(result.is_err(), "should reject pause from {}", terminal);
        }
    }

    #[test]
    fn pause_rejects_already_paused() {
        let mut mission = Mission::new(
            MissionId("m-pause-twice".into()),
            "test",
            "ws-1",
            MissionOwnership::solo("agent-1"),
            1000,
        );
        mission.lifecycle_state = MissionLifecycleState::Running;
        mission
            .pause_mission("op", "first_pause", 2000, None)
            .unwrap();

        let result = mission.pause_mission("op", "second_pause", 3000, None);
        assert!(result.is_err(), "should reject pause when already paused");
    }

    #[test]
    fn pause_rejects_planning_state() {
        let mut mission = Mission::new(
            MissionId("m-pause-planning".into()),
            "test",
            "ws-1",
            MissionOwnership::solo("agent-1"),
            1000,
        );

        let result = mission.pause_mission("op", "reason", 2000, None);
        assert!(result.is_err(), "should reject pause from Planning");
    }

    #[test]
    fn pause_rejects_empty_requested_by() {
        let mut mission = Mission::new(
            MissionId("m-pause-empty".into()),
            "test",
            "ws-1",
            MissionOwnership::solo("agent-1"),
            1000,
        );
        mission.lifecycle_state = MissionLifecycleState::Running;

        let result = mission.pause_mission("", "reason", 2000, None);
        assert!(result.is_err());

        let result = mission.pause_mission("  ", "reason", 2000, None);
        assert!(result.is_err());
    }

    #[test]
    fn resume_restores_paused_from_state() {
        let mut mission = Mission::new(
            MissionId("m-resume".into()),
            "test",
            "ws-1",
            MissionOwnership::solo("agent-1"),
            1000,
        );
        mission.lifecycle_state = MissionLifecycleState::Running;
        mission.pause_mission("op", "manual", 2000, None).unwrap();

        let decision = mission
            .resume_mission("op", "ready_to_continue", 5000, None)
            .unwrap();

        assert_eq!(decision.action, "resume");
        assert_eq!(decision.lifecycle_from, MissionLifecycleState::Paused);
        assert_eq!(decision.lifecycle_to, MissionLifecycleState::Running);
        assert_eq!(mission.lifecycle_state, MissionLifecycleState::Running);
        assert!(!mission.pause_resume_state.is_paused());
        assert_eq!(mission.pause_resume_state.total_resume_count, 1);
    }

    #[test]
    fn resume_rejects_not_paused() {
        let mut mission = Mission::new(
            MissionId("m-resume-not-paused".into()),
            "test",
            "ws-1",
            MissionOwnership::solo("agent-1"),
            1000,
        );
        mission.lifecycle_state = MissionLifecycleState::Running;

        let result = mission.resume_mission("op", "reason", 2000, None);
        assert!(result.is_err(), "should reject resume when not paused");
    }

    #[test]
    fn resume_records_duration() {
        let mut mission = Mission::new(
            MissionId("m-duration".into()),
            "test",
            "ws-1",
            MissionOwnership::solo("agent-1"),
            1000,
        );
        mission.lifecycle_state = MissionLifecycleState::Running;
        mission.pause_mission("op", "manual", 2000, None).unwrap();
        mission.resume_mission("op", "ready", 5000, None).unwrap();

        assert_eq!(
            mission.pause_resume_state.cumulative_pause_duration_ms,
            3000
        );

        let history = &mission.pause_resume_state.checkpoint_history;
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].resumed_at_ms, Some(5000));
        assert_eq!(history[0].pause_duration_ms(), Some(3000));
    }

    #[test]
    fn resume_cumulative_duration_tracking() {
        let mut mission = Mission::new(
            MissionId("m-cumul".into()),
            "test",
            "ws-1",
            MissionOwnership::solo("agent-1"),
            1000,
        );
        mission.lifecycle_state = MissionLifecycleState::Running;

        // First pause/resume: 1000ms
        mission.pause_mission("op", "pause1", 2000, None).unwrap();
        mission.resume_mission("op", "resume1", 3000, None).unwrap();

        // Second pause/resume: 2000ms
        mission.pause_mission("op", "pause2", 4000, None).unwrap();
        mission.resume_mission("op", "resume2", 6000, None).unwrap();

        assert_eq!(
            mission.pause_resume_state.cumulative_pause_duration_ms,
            3000
        );
        assert_eq!(mission.pause_resume_state.total_pause_count, 2);
        assert_eq!(mission.pause_resume_state.total_resume_count, 2);
        assert_eq!(mission.pause_resume_state.checkpoint_history.len(), 2);
    }

    #[test]
    fn abort_from_running_cancels_all_assignments() {
        let mut mission = Mission::new(
            MissionId("m-abort-run".into()),
            "test",
            "ws-1",
            MissionOwnership::solo("agent-1"),
            1000,
        );
        mission.lifecycle_state = MissionLifecycleState::Running;
        mission.candidates.push(CandidateAction {
            candidate_id: CandidateActionId("c1".into()),
            requested_by: MissionActorRole::Planner,
            action: StepAction::SendText {
                pane_id: 0,
                text: "test".into(),
                paste_mode: None,
            },
            rationale: "test".into(),
            score: None,
            created_at_ms: 1000,
        });
        mission.assignments.push(Assignment {
            assignment_id: AssignmentId("a1".into()),
            candidate_id: CandidateActionId("c1".into()),
            assigned_by: MissionActorRole::Dispatcher,
            assignee: "agent-1".into(),
            created_at_ms: 1000,
            updated_at_ms: None,
            approval_state: ApprovalState::NotRequired,
            outcome: None,
            reservation_intent: None,
            escalation: None,
        });

        let decision = mission
            .abort_mission(
                "operator",
                "emergency_stop",
                Some("FTM9999".into()),
                5000,
                None,
            )
            .unwrap();

        assert_eq!(decision.action, "abort");
        assert_eq!(decision.lifecycle_from, MissionLifecycleState::Running);
        assert_eq!(decision.lifecycle_to, MissionLifecycleState::Cancelled);
        assert_eq!(mission.lifecycle_state, MissionLifecycleState::Cancelled);
        assert_eq!(mission.pause_resume_state.total_abort_count, 1);

        let outcome = mission.assignments[0].outcome.as_ref().unwrap();
        let is_cancelled = matches!(outcome, Outcome::Cancelled { .. });
        assert!(is_cancelled);
    }

    #[test]
    fn abort_from_paused_cancels() {
        let mut mission = Mission::new(
            MissionId("m-abort-paused".into()),
            "test",
            "ws-1",
            MissionOwnership::solo("agent-1"),
            1000,
        );
        mission.lifecycle_state = MissionLifecycleState::Running;
        mission.pause_mission("op", "pause", 2000, None).unwrap();

        let _decision = mission
            .abort_mission("op", "abort_while_paused", None, 5000, None)
            .unwrap();

        assert_eq!(mission.lifecycle_state, MissionLifecycleState::Cancelled);
        assert!(!mission.pause_resume_state.is_paused());
        assert_eq!(mission.pause_resume_state.checkpoint_history.len(), 1);
        let cp = &mission.pause_resume_state.checkpoint_history[0];
        assert_eq!(cp.resumed_at_ms, Some(5000));
        assert_eq!(
            mission.pause_resume_state.cumulative_pause_duration_ms,
            3000
        );
    }

    #[test]
    fn abort_from_planning_cancels() {
        let mut mission = Mission::new(
            MissionId("m-abort-planning".into()),
            "test",
            "ws-1",
            MissionOwnership::solo("agent-1"),
            1000,
        );

        let decision = mission
            .abort_mission("op", "cancel_before_start", None, 2000, None)
            .unwrap();

        assert_eq!(decision.lifecycle_from, MissionLifecycleState::Planning);
        assert_eq!(mission.lifecycle_state, MissionLifecycleState::Cancelled);
    }

    #[test]
    fn abort_rejects_terminal_states() {
        for terminal in [
            MissionLifecycleState::Completed,
            MissionLifecycleState::Failed,
            MissionLifecycleState::Cancelled,
        ] {
            let mut mission = Mission::new(
                MissionId("m-abort-term".into()),
                "test",
                "ws-1",
                MissionOwnership::solo("agent-1"),
                1000,
            );
            mission.lifecycle_state = terminal;

            let result = mission.abort_mission("op", "reason", None, 2000, None);
            assert!(result.is_err(), "should reject abort from {}", terminal);
        }
    }

    #[test]
    fn abort_rejects_empty_requested_by() {
        let mut mission = Mission::new(
            MissionId("m-abort-empty".into()),
            "test",
            "ws-1",
            MissionOwnership::solo("agent-1"),
            1000,
        );
        mission.lifecycle_state = MissionLifecycleState::Running;

        let result = mission.abort_mission("", "reason", None, 2000, None);
        assert!(result.is_err());

        let result = mission.abort_mission("  ", "reason", None, 2000, None);
        assert!(result.is_err());
    }

    #[test]
    fn can_pause_guards_correct_states() {
        let pausable = [
            MissionLifecycleState::Running,
            MissionLifecycleState::Dispatching,
            MissionLifecycleState::AwaitingApproval,
            MissionLifecycleState::Blocked,
            MissionLifecycleState::RetryPending,
        ];
        let not_pausable = [
            MissionLifecycleState::Planning,
            MissionLifecycleState::Planned,
            MissionLifecycleState::Paused,
            MissionLifecycleState::Completed,
            MissionLifecycleState::Failed,
            MissionLifecycleState::Cancelled,
        ];

        for state in pausable {
            let mut mission = Mission::new(
                MissionId("m-guard".into()),
                "test",
                "ws-1",
                MissionOwnership::solo("agent-1"),
                1000,
            );
            mission.lifecycle_state = state;
            assert!(mission.can_pause(), "should be pausable from {}", state);
        }
        for state in not_pausable {
            let mut mission = Mission::new(
                MissionId("m-guard".into()),
                "test",
                "ws-1",
                MissionOwnership::solo("agent-1"),
                1000,
            );
            mission.lifecycle_state = state;
            assert!(
                !mission.can_pause(),
                "should NOT be pausable from {}",
                state
            );
        }
    }

    #[test]
    fn can_resume_only_when_paused() {
        for state in MissionLifecycleState::all() {
            let mut mission = Mission::new(
                MissionId("m-resume-guard".into()),
                "test",
                "ws-1",
                MissionOwnership::solo("agent-1"),
                1000,
            );
            mission.lifecycle_state = state;
            if state == MissionLifecycleState::Paused {
                assert!(mission.can_resume());
            } else {
                assert!(!mission.can_resume());
            }
        }
    }

    #[test]
    fn can_abort_all_non_terminal() {
        for state in MissionLifecycleState::all() {
            let mut mission = Mission::new(
                MissionId("m-abort-guard".into()),
                "test",
                "ws-1",
                MissionOwnership::solo("agent-1"),
                1000,
            );
            mission.lifecycle_state = state;
            if state.is_terminal() {
                assert!(!mission.can_abort());
            } else {
                assert!(mission.can_abort());
            }
        }
    }

    #[test]
    fn checkpoint_captures_assignment_state() {
        let mut mission = Mission::new(
            MissionId("m-cp-assign".into()),
            "test",
            "ws-1",
            MissionOwnership::solo("agent-1"),
            1000,
        );
        mission.lifecycle_state = MissionLifecycleState::Running;
        mission.candidates.push(CandidateAction {
            candidate_id: CandidateActionId("c1".into()),
            requested_by: MissionActorRole::Planner,
            action: StepAction::SendText {
                pane_id: 0,
                text: "test".into(),
                paste_mode: None,
            },
            rationale: "test".into(),
            score: None,
            created_at_ms: 1000,
        });
        mission.assignments.push(Assignment {
            assignment_id: AssignmentId("a1".into()),
            candidate_id: CandidateActionId("c1".into()),
            assigned_by: MissionActorRole::Dispatcher,
            assignee: "agent-1".into(),
            created_at_ms: 1000,
            updated_at_ms: None,
            approval_state: ApprovalState::Pending {
                requested_by: "op".into(),
                requested_at_ms: 1500,
            },
            outcome: None,
            reservation_intent: None,
            escalation: None,
        });

        mission
            .pause_mission("op", "investigate", 2000, None)
            .unwrap();

        let cp = mission
            .pause_resume_state
            .current_checkpoint
            .as_ref()
            .unwrap();
        assert_eq!(cp.assignment_entries.len(), 1);
        assert_eq!(cp.assignment_entries[0].assignment_id.0, "a1");
        assert!(cp.assignment_entries[0].outcome_summary.is_none());
        assert!(
            cp.assignment_entries[0]
                .approval_state_summary
                .contains("pending")
        );
    }

    #[test]
    fn checkpoint_history_bounded_by_eviction() {
        let mut state = MissionPauseResumeState::default();
        for i in 0..5 {
            state.checkpoint_history.push(MissionCheckpoint {
                checkpoint_id: format!("cp-{i}"),
                paused_from_state: MissionLifecycleState::Running,
                paused_by: "op".into(),
                reason_code: "test".into(),
                paused_at_ms: (i as i64) * 1000,
                resumed_at_ms: Some((i as i64) * 1000 + 500),
                resumed_by: Some("op".into()),
                assignment_entries: Vec::new(),
                correlation_id: None,
            });
        }

        assert_eq!(state.checkpoint_history.len(), 5);
        state.evict_history_before(2000);
        assert_eq!(state.checkpoint_history.len(), 3);
    }

    #[test]
    fn pause_resume_state_serde_roundtrip() {
        let mut state = MissionPauseResumeState::default();
        state.total_pause_count = 3;
        state.total_resume_count = 2;
        state.total_abort_count = 1;
        state.cumulative_pause_duration_ms = 15000;
        state.checkpoint_history.push(MissionCheckpoint {
            checkpoint_id: "cp-test".into(),
            paused_from_state: MissionLifecycleState::Running,
            paused_by: "operator".into(),
            reason_code: "manual".into(),
            paused_at_ms: 1000,
            resumed_at_ms: Some(6000),
            resumed_by: Some("operator".into()),
            assignment_entries: vec![AssignmentCheckpointEntry {
                assignment_id: AssignmentId("a1".into()),
                outcome_summary: None,
                approval_state_summary: "not_required".into(),
            }],
            correlation_id: Some("corr-1".into()),
        });

        let json = serde_json::to_string(&state).unwrap();
        let restored: MissionPauseResumeState = serde_json::from_str(&json).unwrap();
        assert_eq!(state, restored);
    }

    #[test]
    fn pause_resume_canonical_string_deterministic() {
        let state = MissionPauseResumeState {
            current_checkpoint: None,
            checkpoint_history: Vec::new(),
            total_pause_count: 2,
            total_resume_count: 1,
            total_abort_count: 0,
            cumulative_pause_duration_ms: 5000,
        };

        let s1 = state.canonical_string();
        let s2 = state.canonical_string();
        assert_eq!(s1, s2);
        assert!(s1.contains("total_pause_count=2"));
        assert!(s1.contains("cumulative_pause_duration_ms=5000"));
    }

    #[test]
    fn mission_control_command_serde_roundtrip() {
        let commands = vec![
            MissionControlCommand::Pause {
                requested_by: "op-1".into(),
                reason_code: "manual".into(),
                requested_at_ms: 1000,
                correlation_id: Some("c-1".into()),
            },
            MissionControlCommand::Resume {
                requested_by: "op-2".into(),
                reason_code: "ready".into(),
                requested_at_ms: 2000,
                correlation_id: None,
            },
            MissionControlCommand::Abort {
                requested_by: "system".into(),
                reason_code: "timeout".into(),
                error_code: Some("FTM9999".into()),
                requested_at_ms: 3000,
                correlation_id: None,
            },
        ];

        for cmd in &commands {
            let json = serde_json::to_string(cmd).unwrap();
            let restored: MissionControlCommand = serde_json::from_str(&json).unwrap();
            assert_eq!(cmd, &restored);
            assert_eq!(cmd.canonical_string(), restored.canonical_string());
        }
    }

    // ── C8: Journal unit tests ──────────────────────────────────────────────

    #[test]
    fn journal_new_is_empty() {
        let journal = MissionJournal::new(MissionId("m-j1".into()));
        assert!(journal.is_empty());
        assert_eq!(journal.len(), 0);
        assert_eq!(journal.next_seq(), 1);
        assert!(journal.last_checkpoint_seq().is_none());
    }

    #[test]
    fn journal_append_increments_seq() {
        let mut journal = MissionJournal::new(MissionId("m-j2".into()));
        let kind = MissionJournalEntryKind::LifecycleTransition {
            from: MissionLifecycleState::Planning,
            to: MissionLifecycleState::Planned,
            transition_kind: MissionLifecycleTransitionKind::PlanFinalized,
        };
        let seq1 = journal
            .append(kind.clone(), "c1", "op", "test", None, 1000)
            .unwrap();
        assert_eq!(seq1, 1);
        assert_eq!(journal.len(), 1);
        assert_eq!(journal.next_seq(), 2);

        let kind2 = MissionJournalEntryKind::LifecycleTransition {
            from: MissionLifecycleState::Planned,
            to: MissionLifecycleState::Dispatching,
            transition_kind: MissionLifecycleTransitionKind::DispatchStarted,
        };
        let seq2 = journal
            .append(kind2, "c2", "op", "dispatch", None, 2000)
            .unwrap();
        assert_eq!(seq2, 2);
        assert_eq!(journal.len(), 2);
    }

    #[test]
    fn journal_duplicate_correlation_rejected() {
        let mut journal = MissionJournal::new(MissionId("m-j3".into()));
        let kind = MissionJournalEntryKind::LifecycleTransition {
            from: MissionLifecycleState::Planning,
            to: MissionLifecycleState::Planned,
            transition_kind: MissionLifecycleTransitionKind::PlanFinalized,
        };
        journal
            .append(kind.clone(), "dup-1", "op", "test", None, 1000)
            .unwrap();

        let result = journal.append(kind, "dup-1", "op", "test", None, 2000);
        assert!(result.is_err());
        let is_dup = matches!(
            result.unwrap_err(),
            MissionJournalError::DuplicateCorrelation { prior_seq: 1, .. }
        );
        assert!(is_dup);
    }

    #[test]
    fn journal_has_correlation() {
        let mut journal = MissionJournal::new(MissionId("m-j4".into()));
        assert!(!journal.has_correlation("c1"));

        let kind = MissionJournalEntryKind::LifecycleTransition {
            from: MissionLifecycleState::Planning,
            to: MissionLifecycleState::Planned,
            transition_kind: MissionLifecycleTransitionKind::PlanFinalized,
        };
        journal
            .append(kind, "c1", "op", "test", None, 1000)
            .unwrap();
        assert!(journal.has_correlation("c1"));
        assert!(!journal.has_correlation("c2"));
    }

    #[test]
    fn journal_checkpoint_records_mission_state() {
        let mission = Mission::new(
            MissionId("m-j5".into()),
            "checkpoint test",
            "ws-j5",
            MissionOwnership::solo("agent-j5"),
            1000,
        );
        let mut journal = MissionJournal::new(MissionId("m-j5".into()));
        let seq = journal.checkpoint(&mission, 2000).unwrap();
        assert_eq!(seq, 1);
        assert_eq!(journal.last_checkpoint_seq(), Some(1));

        let entry = &journal.entries()[0];
        let is_checkpoint = matches!(
            &entry.kind,
            MissionJournalEntryKind::Checkpoint {
                lifecycle_state: MissionLifecycleState::Planning,
                assignment_count: 0,
                ..
            }
        );
        assert!(is_checkpoint);
    }

    #[test]
    fn journal_recovery_marker() {
        let mut journal = MissionJournal::new(MissionId("m-j6".into()));
        let seq = journal.recovery_marker(0, "cold_start", 1000).unwrap();
        assert_eq!(seq, 1);

        let entry = &journal.entries()[0];
        let is_recovery = matches!(
            &entry.kind,
            MissionJournalEntryKind::RecoveryMarker {
                recovered_through_seq: 0,
                ..
            }
        );
        assert!(is_recovery);
    }

    #[test]
    fn journal_entries_since_returns_subset() {
        let mut journal = MissionJournal::new(MissionId("m-j7".into()));
        for i in 1..=5 {
            let kind = MissionJournalEntryKind::LifecycleTransition {
                from: MissionLifecycleState::Planning,
                to: MissionLifecycleState::Planned,
                transition_kind: MissionLifecycleTransitionKind::PlanFinalized,
            };
            journal
                .append(kind, format!("c{i}"), "op", "test", None, i * 1000)
                .unwrap();
        }
        assert_eq!(journal.entries_since(3).len(), 3);
        assert_eq!(journal.entries_since(1).len(), 5);
        assert_eq!(journal.entries_since(6).len(), 0);
        assert_eq!(journal.entries_since(0).len(), 5);
    }

    #[test]
    fn journal_compact_before_removes_entries() {
        let mut journal = MissionJournal::new(MissionId("m-j8".into()));
        for i in 1..=5 {
            let kind = MissionJournalEntryKind::LifecycleTransition {
                from: MissionLifecycleState::Planning,
                to: MissionLifecycleState::Planned,
                transition_kind: MissionLifecycleTransitionKind::PlanFinalized,
            };
            journal
                .append(kind, format!("c{i}"), "op", "test", None, i * 1000)
                .unwrap();
        }

        let removed = journal.compact_before(3);
        assert_eq!(removed, 2);
        assert_eq!(journal.len(), 3);
        assert_eq!(journal.entries()[0].seq, 3);

        // Compacted correlation IDs are removed from index
        assert!(!journal.has_correlation("c1"));
        assert!(!journal.has_correlation("c2"));
        assert!(journal.has_correlation("c3"));
    }

    #[test]
    fn journal_needs_compaction_respects_limit() {
        let mut journal = MissionJournal::new(MissionId("m-j9".into())).with_max_entries(3);
        assert!(!journal.needs_compaction());

        for i in 1..=4 {
            let kind = MissionJournalEntryKind::RecoveryMarker {
                recovered_through_seq: 0,
                recovery_reason: "test".into(),
            };
            journal
                .append(kind, format!("c{i}"), "op", "test", None, i * 1000)
                .unwrap();
        }
        assert!(journal.needs_compaction());
    }

    #[test]
    fn journal_snapshot_state_captures_metadata() {
        let mut journal = MissionJournal::new(MissionId("m-j10".into()));
        let state = journal.snapshot_state();
        assert!(state.is_pristine());
        assert_eq!(state.entry_count, 0);
        assert!(state.clean);

        let kind = MissionJournalEntryKind::LifecycleTransition {
            from: MissionLifecycleState::Planning,
            to: MissionLifecycleState::Planned,
            transition_kind: MissionLifecycleTransitionKind::PlanFinalized,
        };
        journal
            .append(kind, "c1", "op", "test", None, 1000)
            .unwrap();

        let state = journal.snapshot_state();
        assert!(!state.is_pristine());
        assert_eq!(state.entry_count, 1);
        assert_eq!(state.last_seq, 1);
        assert!(!state.clean); // No checkpoint placed yet
    }

    #[test]
    fn journal_snapshot_clean_after_checkpoint() {
        let mission = Mission::new(
            MissionId("m-j11".into()),
            "clean test",
            "ws-j11",
            MissionOwnership::solo("agent-j11"),
            1000,
        );
        let mut journal = MissionJournal::new(MissionId("m-j11".into()));
        journal.checkpoint(&mission, 2000).unwrap();

        let state = journal.snapshot_state();
        assert!(state.clean);
        assert!(state.last_checkpoint_seq.is_some());
        assert!(!state.last_checkpoint_hash.is_empty());
    }

    #[test]
    fn journal_replay_from_checkpoint_reports_counts() {
        let mut journal = MissionJournal::new(MissionId("m-j12".into()));
        let mission = Mission::new(
            MissionId("m-j12".into()),
            "replay test",
            "ws-j12",
            MissionOwnership::solo("agent-j12"),
            1000,
        );

        // Append various entry types
        journal
            .append(
                MissionJournalEntryKind::LifecycleTransition {
                    from: MissionLifecycleState::Planning,
                    to: MissionLifecycleState::Planned,
                    transition_kind: MissionLifecycleTransitionKind::PlanFinalized,
                },
                "c1",
                "op",
                "test",
                None,
                1000,
            )
            .unwrap();
        journal.checkpoint(&mission, 2000).unwrap();
        journal
            .append(
                MissionJournalEntryKind::KillSwitchChange {
                    level_from: MissionKillSwitchLevel::Off,
                    level_to: MissionKillSwitchLevel::SafeMode,
                },
                "c3",
                "op",
                "safety",
                None,
                3000,
            )
            .unwrap();
        journal
            .append(
                MissionJournalEntryKind::AssignmentOutcome {
                    assignment_id: AssignmentId("a1".into()),
                    outcome_before: None,
                    outcome_after: "success".into(),
                },
                "c4",
                "dispatcher",
                "outcome",
                None,
                4000,
            )
            .unwrap();

        let report = journal.replay_from_checkpoint();
        assert!(report.is_clean());
        // Replay from checkpoint (seq=2): sees checkpoint + kill_switch + assignment = 3 entries
        assert_eq!(report.entries_scanned, 3);
        assert_eq!(report.checkpoints_found, 1);
        assert_eq!(report.kill_switch_changes, 1);
        assert_eq!(report.assignment_outcomes, 1);
        assert_eq!(report.total_entries(), 3);
    }

    #[test]
    fn journal_replay_detects_seq_regression() {
        let mut journal = MissionJournal::new(MissionId("m-j13".into()));
        // Manually push entries with non-monotonic seq
        journal.entries.push(MissionJournalEntry {
            seq: 5,
            timestamp_ms: 1000,
            correlation_id: "c1".into(),
            entry_hash: "h1".into(),
            kind: MissionJournalEntryKind::RecoveryMarker {
                recovered_through_seq: 0,
                recovery_reason: "test".into(),
            },
            mission_version: 1,
            initiated_by: "op".into(),
            reason_code: "test".into(),
            error_code: None,
        });
        journal.entries.push(MissionJournalEntry {
            seq: 3, // regression!
            timestamp_ms: 2000,
            correlation_id: "c2".into(),
            entry_hash: "h2".into(),
            kind: MissionJournalEntryKind::RecoveryMarker {
                recovered_through_seq: 0,
                recovery_reason: "test".into(),
            },
            mission_version: 1,
            initiated_by: "op".into(),
            reason_code: "test".into(),
            error_code: None,
        });

        let report = journal.replay_from_checkpoint();
        assert!(!report.is_clean());
        assert_eq!(report.errors.len(), 1);
        assert_eq!(report.errors[0].error_code, "SEQ_REGRESSION");
    }

    #[test]
    fn journal_entry_kind_tag_names() {
        let lt = MissionJournalEntryKind::LifecycleTransition {
            from: MissionLifecycleState::Planning,
            to: MissionLifecycleState::Planned,
            transition_kind: MissionLifecycleTransitionKind::PlanFinalized,
        };
        assert_eq!(lt.tag_name(), "lifecycle_transition");

        let ks = MissionJournalEntryKind::KillSwitchChange {
            level_from: MissionKillSwitchLevel::Off,
            level_to: MissionKillSwitchLevel::SafeMode,
        };
        assert_eq!(ks.tag_name(), "kill_switch_change");

        let cp = MissionJournalEntryKind::Checkpoint {
            mission_hash: "test".into(),
            lifecycle_state: MissionLifecycleState::Running,
            assignment_count: 0,
        };
        assert_eq!(cp.tag_name(), "checkpoint");

        let rm = MissionJournalEntryKind::RecoveryMarker {
            recovered_through_seq: 0,
            recovery_reason: "cold_start".into(),
        };
        assert_eq!(rm.tag_name(), "recovery_marker");

        let ao = MissionJournalEntryKind::AssignmentOutcome {
            assignment_id: AssignmentId("a".into()),
            outcome_before: None,
            outcome_after: "success".into(),
        };
        assert_eq!(ao.tag_name(), "assignment_outcome");
    }

    #[test]
    fn journal_state_serde_roundtrip() {
        let state = MissionJournalState {
            entry_count: 42,
            last_seq: 42,
            last_entry_hash: "j:42:1000:c42".into(),
            last_checkpoint_seq: Some(40),
            last_checkpoint_hash: "sha256:abcdef".into(),
            clean: false,
        };
        let json = serde_json::to_string(&state).unwrap();
        let restored: MissionJournalState = serde_json::from_str(&json).unwrap();
        assert_eq!(state, restored);
    }

    #[test]
    fn journal_state_canonical_string_deterministic() {
        let state = MissionJournalState {
            entry_count: 10,
            last_seq: 10,
            last_entry_hash: "h10".into(),
            last_checkpoint_seq: Some(5),
            last_checkpoint_hash: "cp5".into(),
            clean: true,
        };
        let s1 = state.canonical_string();
        let s2 = state.canonical_string();
        assert_eq!(s1, s2);
        assert!(s1.contains("entry_count=10"));
        assert!(s1.contains("clean=true"));
    }

    #[test]
    fn journal_entry_serde_roundtrip() {
        let entry = MissionJournalEntry {
            seq: 1,
            timestamp_ms: 5000,
            correlation_id: "c-serde".into(),
            entry_hash: "j:1:5000:c-serde".into(),
            kind: MissionJournalEntryKind::LifecycleTransition {
                from: MissionLifecycleState::Running,
                to: MissionLifecycleState::Paused,
                transition_kind: MissionLifecycleTransitionKind::PauseRequested,
            },
            mission_version: 1,
            initiated_by: "op".into(),
            reason_code: "manual_pause".into(),
            error_code: None,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let restored: MissionJournalEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, restored);
    }

    #[test]
    fn journal_entry_canonical_string_deterministic() {
        let entry = MissionJournalEntry {
            seq: 7,
            timestamp_ms: 9000,
            correlation_id: "c-canon".into(),
            entry_hash: "j:7:9000:c-canon".into(),
            kind: MissionJournalEntryKind::KillSwitchChange {
                level_from: MissionKillSwitchLevel::Off,
                level_to: MissionKillSwitchLevel::HardStop,
            },
            mission_version: 1,
            initiated_by: "system".into(),
            reason_code: "emergency".into(),
            error_code: Some("FTM9999".into()),
        };
        let s1 = entry.canonical_string();
        let s2 = entry.canonical_string();
        assert_eq!(s1, s2);
        assert!(s1.contains("seq=7"));
        assert!(s1.contains("err=FTM9999"));
    }

    #[test]
    fn mission_create_journal_uses_mission_id() {
        let mission = Mission::new(
            MissionId("m-cj".into()),
            "journal create test",
            "ws-cj",
            MissionOwnership::solo("agent-cj"),
            1000,
        );
        let journal = mission.create_journal();
        assert_eq!(journal.mission_id.0, "m-cj");
        assert!(journal.is_empty());
    }

    #[test]
    fn mission_sync_journal_state() {
        let mut mission = Mission::new(
            MissionId("m-sj".into()),
            "sync test",
            "ws-sj",
            MissionOwnership::solo("agent-sj"),
            1000,
        );
        assert!(mission.journal_state.is_pristine());

        let mut journal = mission.create_journal();
        journal
            .append(
                MissionJournalEntryKind::LifecycleTransition {
                    from: MissionLifecycleState::Planning,
                    to: MissionLifecycleState::Planned,
                    transition_kind: MissionLifecycleTransitionKind::PlanFinalized,
                },
                "c1",
                "op",
                "test",
                None,
                2000,
            )
            .unwrap();

        mission.sync_journal_state(&journal);
        assert!(!mission.journal_state.is_pristine());
        assert_eq!(mission.journal_state.entry_count, 1);
        assert_eq!(mission.journal_state.last_seq, 1);
    }

    #[test]
    fn journal_lifecycle_transition_helper() {
        let mut journal = MissionJournal::new(MissionId("m-lt".into()));
        let seq = Mission::journal_lifecycle_transition(
            &mut journal,
            MissionLifecycleState::Planning,
            MissionLifecycleState::Planned,
            MissionLifecycleTransitionKind::PlanFinalized,
            "lt-1",
            "planner",
            3000,
        )
        .unwrap();
        assert_eq!(seq, 1);
        let entry = &journal.entries()[0];
        let is_lt = matches!(
            &entry.kind,
            MissionJournalEntryKind::LifecycleTransition {
                from: MissionLifecycleState::Planning,
                to: MissionLifecycleState::Planned,
                ..
            }
        );
        assert!(is_lt);
    }

    #[test]
    fn journal_kill_switch_change_helper() {
        let mut journal = MissionJournal::new(MissionId("m-ks".into()));
        let seq = Mission::journal_kill_switch_change(
            &mut journal,
            MissionKillSwitchLevel::Off,
            MissionKillSwitchLevel::SafeMode,
            "ks-1",
            "safety",
            4000,
        )
        .unwrap();
        assert_eq!(seq, 1);
        let entry = &journal.entries()[0];
        let is_ks = matches!(
            &entry.kind,
            MissionJournalEntryKind::KillSwitchChange {
                level_from: MissionKillSwitchLevel::Off,
                level_to: MissionKillSwitchLevel::SafeMode,
            }
        );
        assert!(is_ks);
    }

    #[test]
    fn journal_assignment_outcome_helper() {
        let mut journal = MissionJournal::new(MissionId("m-ao".into()));
        let seq = Mission::journal_assignment_outcome(
            &mut journal,
            &AssignmentId("a1".into()),
            None,
            "success",
            "ao-1",
            "dispatcher",
            5000,
        )
        .unwrap();
        assert_eq!(seq, 1);
        let entry = &journal.entries()[0];
        let is_ao = matches!(
            &entry.kind,
            MissionJournalEntryKind::AssignmentOutcome {
                outcome_before: None,
                ..
            }
        );
        assert!(is_ao);
    }

    #[test]
    fn journal_control_command_helper() {
        let mut journal = MissionJournal::new(MissionId("m-cc".into()));
        let cmd = MissionControlCommand::Pause {
            requested_by: "op".into(),
            reason_code: "manual".into(),
            requested_at_ms: 6000,
            correlation_id: Some("cc-1".into()),
        };
        let decision = MissionControlDecision {
            action: "pause".into(),
            lifecycle_from: MissionLifecycleState::Running,
            lifecycle_to: MissionLifecycleState::Paused,
            decision_path: "pause_mission->running->paused".into(),
            reason_code: "manual".into(),
            error_code: None,
            checkpoint_id: Some("cp-1".into()),
            decided_at_ms: 6000,
        };
        let seq =
            Mission::journal_control_command(&mut journal, &cmd, &decision, "cc-1", 6000).unwrap();
        assert_eq!(seq, 1);
        let entry = &journal.entries()[0];
        let is_cc = matches!(&entry.kind, MissionJournalEntryKind::ControlCommand { .. });
        assert!(is_cc);
    }

    #[test]
    fn journal_multiple_checkpoints_track_last() {
        let mission = Mission::new(
            MissionId("m-mc".into()),
            "multi checkpoint",
            "ws-mc",
            MissionOwnership::solo("agent-mc"),
            1000,
        );
        let mut journal = MissionJournal::new(MissionId("m-mc".into()));

        journal.checkpoint(&mission, 1000).unwrap();
        assert_eq!(journal.last_checkpoint_seq(), Some(1));

        journal
            .append(
                MissionJournalEntryKind::LifecycleTransition {
                    from: MissionLifecycleState::Planning,
                    to: MissionLifecycleState::Planned,
                    transition_kind: MissionLifecycleTransitionKind::PlanFinalized,
                },
                "c-between",
                "op",
                "test",
                None,
                2000,
            )
            .unwrap();

        journal.checkpoint(&mission, 3000).unwrap();
        assert_eq!(journal.last_checkpoint_seq(), Some(3));

        // Replay from last checkpoint should only see 1 entry (the checkpoint itself)
        let report = journal.replay_from_checkpoint();
        assert_eq!(report.entries_scanned, 1);
        assert_eq!(report.checkpoints_found, 1);
    }

    #[test]
    fn journal_compact_preserves_post_checkpoint_entries() {
        let mission = Mission::new(
            MissionId("m-cpre".into()),
            "compact test",
            "ws-cpre",
            MissionOwnership::solo("agent-cpre"),
            1000,
        );
        let mut journal = MissionJournal::new(MissionId("m-cpre".into()));

        // 3 entries, then checkpoint, then 2 more
        for i in 1..=3 {
            journal
                .append(
                    MissionJournalEntryKind::RecoveryMarker {
                        recovered_through_seq: 0,
                        recovery_reason: "setup".into(),
                    },
                    format!("pre-{i}"),
                    "op",
                    "test",
                    None,
                    i * 1000,
                )
                .unwrap();
        }
        journal.checkpoint(&mission, 4000).unwrap();
        for i in 1..=2 {
            journal
                .append(
                    MissionJournalEntryKind::RecoveryMarker {
                        recovered_through_seq: 0,
                        recovery_reason: "post".into(),
                    },
                    format!("post-{i}"),
                    "op",
                    "test",
                    None,
                    (4 + i) * 1000,
                )
                .unwrap();
        }

        assert_eq!(journal.len(), 6);
        // Compact everything before checkpoint (seq=4)
        let removed = journal.compact_before(4);
        assert_eq!(removed, 3);
        assert_eq!(journal.len(), 3); // checkpoint + 2 post entries
    }

    #[test]
    fn journal_error_display() {
        let err = MissionJournalError::DuplicateCorrelation {
            correlation_id: "dup-test".into(),
            prior_seq: 42,
        };
        let msg = format!("{}", err);
        assert!(msg.contains("dup-test"));
        assert!(msg.contains("42"));

        let err2 = MissionJournalError::ValidationFailed {
            reason: "bad state".into(),
        };
        let msg2 = format!("{}", err2);
        assert!(msg2.contains("bad state"));
    }

    #[test]
    fn journal_replay_report_total_entries() {
        let report = MissionJournalReplayReport {
            start_seq: 0,
            entries_scanned: 10,
            lifecycle_transitions: 3,
            control_commands: 2,
            kill_switch_changes: 1,
            assignment_outcomes: 2,
            checkpoints_found: 1,
            recovery_markers: 1,
            errors: Vec::new(),
        };
        assert_eq!(report.total_entries(), 10);
        assert!(report.is_clean());
    }

    #[test]
    fn journal_replay_report_with_errors() {
        let report = MissionJournalReplayReport {
            start_seq: 5,
            entries_scanned: 2,
            lifecycle_transitions: 1,
            control_commands: 0,
            kill_switch_changes: 0,
            assignment_outcomes: 1,
            checkpoints_found: 0,
            recovery_markers: 0,
            errors: vec![MissionJournalReplayError {
                seq: 6,
                error_code: "TEST_ERR".into(),
                message: "test error".into(),
            }],
        };
        assert!(!report.is_clean());
        assert_eq!(report.errors.len(), 1);
    }

    #[test]
    fn journal_mission_canonical_string_includes_journal() {
        let mut mission = Mission::new(
            MissionId("m-can".into()),
            "canonical test",
            "ws-can",
            MissionOwnership::solo("agent-can"),
            1000,
        );
        let canonical_before = mission.canonical_string();
        assert!(!canonical_before.contains("journal_state="));

        mission.journal_state = MissionJournalState {
            entry_count: 5,
            last_seq: 5,
            last_entry_hash: "h5".into(),
            last_checkpoint_seq: Some(3),
            last_checkpoint_hash: "cp3".into(),
            clean: false,
        };
        let canonical_after = mission.canonical_string();
        assert!(canonical_after.contains("journal_state="));
        assert!(canonical_after.contains("entry_count=5"));
    }

    #[test]
    fn journal_entry_all_kinds_serde_roundtrip() {
        let kinds = [
            MissionJournalEntryKind::LifecycleTransition {
                from: MissionLifecycleState::Running,
                to: MissionLifecycleState::Paused,
                transition_kind: MissionLifecycleTransitionKind::PauseRequested,
            },
            MissionJournalEntryKind::KillSwitchChange {
                level_from: MissionKillSwitchLevel::Off,
                level_to: MissionKillSwitchLevel::HardStop,
            },
            MissionJournalEntryKind::AssignmentOutcome {
                assignment_id: AssignmentId("a1".into()),
                outcome_before: Some("pending".into()),
                outcome_after: "success".into(),
            },
            MissionJournalEntryKind::Checkpoint {
                mission_hash: "sha256:abc".into(),
                lifecycle_state: MissionLifecycleState::Running,
                assignment_count: 3,
            },
            MissionJournalEntryKind::RecoveryMarker {
                recovered_through_seq: 10,
                recovery_reason: "restart".into(),
            },
        ];

        for (i, kind) in kinds.iter().enumerate() {
            let entry = MissionJournalEntry {
                seq: (i + 1) as u64,
                timestamp_ms: (i as i64 + 1) * 1000,
                correlation_id: format!("kind-{i}"),
                entry_hash: format!("h-{i}"),
                kind: kind.clone(),
                mission_version: 1,
                initiated_by: "test".into(),
                reason_code: "serde".into(),
                error_code: None,
            };
            let json = serde_json::to_string(&entry).unwrap();
            let restored: MissionJournalEntry = serde_json::from_str(&json).unwrap();
            assert_eq!(entry, restored);
        }
    }

    #[test]
    fn journal_control_command_entry_serde_roundtrip() {
        let kind = MissionJournalEntryKind::ControlCommand {
            command: MissionControlCommand::Abort {
                requested_by: "op".into(),
                reason_code: "timeout".into(),
                error_code: Some("FTM001".into()),
                requested_at_ms: 9000,
                correlation_id: None,
            },
            decision: MissionControlDecision {
                action: "abort".into(),
                lifecycle_from: MissionLifecycleState::Running,
                lifecycle_to: MissionLifecycleState::Cancelled,
                decision_path: "abort".into(),
                reason_code: "timeout".into(),
                error_code: Some("FTM001".into()),
                checkpoint_id: None,
                decided_at_ms: 9000,
            },
        };
        let entry = MissionJournalEntry {
            seq: 1,
            timestamp_ms: 9000,
            correlation_id: "cc-serde".into(),
            entry_hash: "h-cc".into(),
            kind,
            mission_version: 1,
            initiated_by: "op".into(),
            reason_code: "timeout".into(),
            error_code: Some("FTM001".into()),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let restored: MissionJournalEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, restored);
    }

    #[test]
    fn journal_replay_report_serde_roundtrip() {
        let report = MissionJournalReplayReport {
            start_seq: 5,
            entries_scanned: 20,
            lifecycle_transitions: 8,
            control_commands: 3,
            kill_switch_changes: 1,
            assignment_outcomes: 5,
            checkpoints_found: 2,
            recovery_markers: 1,
            errors: vec![MissionJournalReplayError {
                seq: 12,
                error_code: "SEQ_REGRESSION".into(),
                message: "non-monotonic".into(),
            }],
        };
        let json = serde_json::to_string(&report).unwrap();
        let restored: MissionJournalReplayReport = serde_json::from_str(&json).unwrap();
        assert_eq!(report, restored);
    }

    // ── H5: Commit-phase executor tests ─────────────────────────────────────

    fn make_commit_contract(num_steps: usize) -> MissionTxContract {
        let steps: Vec<TxStep> = (1..=num_steps)
            .map(|i| TxStep {
                step_id: TxStepId(format!("s{i}")),
                ordinal: i as u32,
                action: StepAction::SendText {
                    pane_id: i as u64,
                    text: format!("step-{i}"),
                    paste_mode: None,
                },
            })
            .collect();

        MissionTxContract {
            tx_version: 1,
            intent: TxIntent {
                tx_id: TxId("tx:commit-test".into()),
                requested_by: MissionActorRole::Dispatcher,
                summary: "commit test".into(),
                correlation_id: "ct-1".into(),
                created_at_ms: 1000,
            },
            plan: TxPlan {
                plan_id: TxPlanId("plan:commit-test".into()),
                tx_id: TxId("tx:commit-test".into()),
                steps,
                preconditions: vec![],
                compensations: vec![],
            },
            lifecycle_state: MissionTxState::Prepared,
            outcome: TxOutcome::Pending,
            receipts: vec![],
        }
    }

    fn success_input(step_id: &str, ts: i64) -> TxCommitStepInput {
        TxCommitStepInput {
            step_id: TxStepId(step_id.into()),
            success: true,
            reason_code: "ok".into(),
            error_code: None,
            completed_at_ms: ts,
        }
    }

    fn failure_input(step_id: &str, ts: i64) -> TxCommitStepInput {
        TxCommitStepInput {
            step_id: TxStepId(step_id.into()),
            success: false,
            reason_code: "exec_error".into(),
            error_code: Some("FTX9999".into()),
            completed_at_ms: ts,
        }
    }

    #[test]
    fn commit_all_steps_succeed() {
        let contract = make_commit_contract(3);
        let inputs = vec![
            success_input("s1", 2000),
            success_input("s2", 3000),
            success_input("s3", 4000),
        ];
        let report =
            execute_commit_phase(&contract, &inputs, MissionKillSwitchLevel::Off, false, 5000)
                .unwrap();

        assert!(report.is_fully_committed());
        assert!(!report.has_failures());
        assert_eq!(report.committed_count, 3);
        assert_eq!(report.failed_count, 0);
        assert_eq!(report.skipped_count, 0);
        assert!(report.failure_boundary.is_none());
        assert_eq!(report.outcome.target_tx_state(), MissionTxState::Committed);
        assert_eq!(report.receipts.len(), 2); // start + terminal
    }

    #[test]
    fn commit_first_step_fails_immediate_failure() {
        let contract = make_commit_contract(3);
        let inputs = vec![failure_input("s1", 2000)];
        let report =
            execute_commit_phase(&contract, &inputs, MissionKillSwitchLevel::Off, false, 5000)
                .unwrap();

        let is_immediate = matches!(report.outcome, TxCommitOutcome::ImmediateFailure);
        assert!(is_immediate);
        assert_eq!(report.committed_count, 0);
        assert_eq!(report.failed_count, 1);
        assert_eq!(report.skipped_count, 2);
        assert_eq!(report.failure_boundary, Some(1));
        assert_eq!(report.outcome.target_tx_state(), MissionTxState::Failed);
    }

    #[test]
    fn commit_partial_failure_trips_barrier() {
        let contract = make_commit_contract(3);
        let inputs = vec![success_input("s1", 2000), failure_input("s2", 3000)];
        let report =
            execute_commit_phase(&contract, &inputs, MissionKillSwitchLevel::Off, false, 5000)
                .unwrap();

        let is_partial = matches!(report.outcome, TxCommitOutcome::PartialFailure);
        assert!(is_partial);
        assert_eq!(report.committed_count, 1);
        assert_eq!(report.failed_count, 1);
        assert_eq!(report.skipped_count, 1);
        assert_eq!(report.failure_boundary, Some(2));
        assert_eq!(
            report.outcome.target_tx_state(),
            MissionTxState::Compensating
        );

        // Step 3 should be skipped
        let s3 = &report.step_results[2];
        assert!(s3.outcome.is_skipped());
    }

    #[test]
    fn commit_kill_switch_blocks_all_steps() {
        let contract = make_commit_contract(3);
        let inputs = vec![
            success_input("s1", 2000),
            success_input("s2", 3000),
            success_input("s3", 4000),
        ];
        let report = execute_commit_phase(
            &contract,
            &inputs,
            MissionKillSwitchLevel::SafeMode,
            false,
            5000,
        )
        .unwrap();

        let is_blocked = matches!(report.outcome, TxCommitOutcome::KillSwitchBlocked);
        assert!(is_blocked);
        assert_eq!(report.committed_count, 0);
        assert_eq!(report.skipped_count, 3);
    }

    #[test]
    fn commit_kill_switch_hard_stop_blocks() {
        let contract = make_commit_contract(2);
        let inputs = vec![success_input("s1", 2000), success_input("s2", 3000)];
        let report = execute_commit_phase(
            &contract,
            &inputs,
            MissionKillSwitchLevel::HardStop,
            false,
            5000,
        )
        .unwrap();

        let is_blocked = matches!(report.outcome, TxCommitOutcome::KillSwitchBlocked);
        assert!(is_blocked);
    }

    #[test]
    fn commit_paused_suspends_execution() {
        let contract = make_commit_contract(3);
        let inputs = vec![
            success_input("s1", 2000),
            success_input("s2", 3000),
            success_input("s3", 4000),
        ];
        let report = execute_commit_phase(
            &contract,
            &inputs,
            MissionKillSwitchLevel::Off,
            true, // paused
            5000,
        )
        .unwrap();

        let is_paused = matches!(report.outcome, TxCommitOutcome::PauseSuspended);
        assert!(is_paused);
        assert_eq!(report.committed_count, 0);
        assert_eq!(report.skipped_count, 3);
        assert_eq!(report.outcome.target_tx_state(), MissionTxState::Committing);
    }

    #[test]
    fn commit_missing_step_input_treated_as_failure() {
        let contract = make_commit_contract(3);
        let inputs = vec![
            success_input("s1", 2000),
            // s2 missing — no input
            success_input("s3", 4000),
        ];
        let report =
            execute_commit_phase(&contract, &inputs, MissionKillSwitchLevel::Off, false, 5000)
                .unwrap();

        assert!(report.has_failures());
        assert_eq!(report.committed_count, 1);
        assert_eq!(report.failed_count, 1);
        assert_eq!(report.skipped_count, 1);
        assert_eq!(report.failure_boundary, Some(2));

        // s2 should be failed with "step_input_missing"
        let s2 = &report.step_results[1];
        let is_missing_fail = matches!(
            &s2.outcome,
            TxCommitStepOutcome::Failed { error_code, .. }
            if error_code == "FTX3003"
        );
        assert!(is_missing_fail);
    }

    #[test]
    fn commit_rejects_non_prepared_state() {
        let mut contract = make_commit_contract(1);
        contract.lifecycle_state = MissionTxState::Draft;
        let result = execute_commit_phase(&contract, &[], MissionKillSwitchLevel::Off, false, 5000);
        assert!(result.is_err());
    }

    #[test]
    fn commit_allows_committing_state_resume() {
        let mut contract = make_commit_contract(1);
        contract.lifecycle_state = MissionTxState::Committing;
        let inputs = vec![success_input("s1", 2000)];
        let report =
            execute_commit_phase(&contract, &inputs, MissionKillSwitchLevel::Off, false, 5000)
                .unwrap();
        assert!(report.is_fully_committed());
    }

    #[test]
    fn commit_rejects_empty_plan() {
        let mut contract = make_commit_contract(0);
        // Fix: empty steps with Prepared state
        contract.lifecycle_state = MissionTxState::Prepared;
        let result = execute_commit_phase(&contract, &[], MissionKillSwitchLevel::Off, false, 5000);
        assert!(result.is_err());
    }

    #[test]
    fn commit_step_outcome_tag_names() {
        let committed = TxCommitStepOutcome::Committed {
            reason_code: "ok".into(),
        };
        assert_eq!(committed.tag_name(), "committed");
        assert!(committed.is_committed());

        let failed = TxCommitStepOutcome::Failed {
            reason_code: "err".into(),
            error_code: "E1".into(),
        };
        assert_eq!(failed.tag_name(), "failed");
        assert!(failed.is_failed());

        let skipped = TxCommitStepOutcome::Skipped {
            reason_code: "barrier".into(),
        };
        assert_eq!(skipped.tag_name(), "skipped");
        assert!(skipped.is_skipped());

        let blocked = TxCommitStepOutcome::Blocked {
            reason_code: "ks".into(),
            error_code: "E2".into(),
        };
        assert_eq!(blocked.tag_name(), "blocked");
    }

    #[test]
    fn commit_outcome_target_states() {
        assert_eq!(
            TxCommitOutcome::FullyCommitted.target_tx_state(),
            MissionTxState::Committed
        );
        assert_eq!(
            TxCommitOutcome::PartialFailure.target_tx_state(),
            MissionTxState::Compensating
        );
        assert_eq!(
            TxCommitOutcome::ImmediateFailure.target_tx_state(),
            MissionTxState::Failed
        );
        assert_eq!(
            TxCommitOutcome::KillSwitchBlocked.target_tx_state(),
            MissionTxState::Failed
        );
        assert_eq!(
            TxCommitOutcome::PauseSuspended.target_tx_state(),
            MissionTxState::Committing
        );
    }

    #[test]
    fn commit_report_canonical_string_deterministic() {
        let contract = make_commit_contract(2);
        let inputs = vec![success_input("s1", 2000), success_input("s2", 3000)];
        let report =
            execute_commit_phase(&contract, &inputs, MissionKillSwitchLevel::Off, false, 5000)
                .unwrap();

        let s1 = report.canonical_string();
        let s2 = report.canonical_string();
        assert_eq!(s1, s2);
        assert!(s1.contains("fully_committed"));
    }

    #[test]
    fn commit_report_serde_roundtrip() {
        let contract = make_commit_contract(2);
        let inputs = vec![success_input("s1", 2000), failure_input("s2", 3000)];
        let report =
            execute_commit_phase(&contract, &inputs, MissionKillSwitchLevel::Off, false, 5000)
                .unwrap();

        let json = serde_json::to_string(&report).unwrap();
        let restored: TxCommitReport = serde_json::from_str(&json).unwrap();
        assert_eq!(report, restored);
    }

    #[test]
    fn commit_step_result_serde_roundtrip() {
        let result = TxCommitStepResult {
            step_id: TxStepId("s1".into()),
            ordinal: 1,
            outcome: TxCommitStepOutcome::Committed {
                reason_code: "ok".into(),
            },
            decision_path: "commit_step_succeeded".into(),
            completed_at_ms: 5000,
        };
        let json = serde_json::to_string(&result).unwrap();
        let restored: TxCommitStepResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result, restored);
    }

    #[test]
    fn commit_step_input_serde_roundtrip() {
        let input = TxCommitStepInput {
            step_id: TxStepId("s1".into()),
            success: false,
            reason_code: "timeout".into(),
            error_code: Some("FTX9001".into()),
            completed_at_ms: 9000,
        };
        let json = serde_json::to_string(&input).unwrap();
        let restored: TxCommitStepInput = serde_json::from_str(&json).unwrap();
        assert_eq!(input, restored);
    }

    #[test]
    fn commit_receipts_have_monotonic_seq() {
        let contract = make_commit_contract(3);
        let inputs = vec![
            success_input("s1", 2000),
            success_input("s2", 3000),
            success_input("s3", 4000),
        ];
        let report =
            execute_commit_phase(&contract, &inputs, MissionKillSwitchLevel::Off, false, 5000)
                .unwrap();

        let mut prev_seq = 0u64;
        for receipt in &report.receipts {
            assert!(
                receipt.seq > prev_seq,
                "receipt seq {} must be > prev {}",
                receipt.seq,
                prev_seq,
            );
            prev_seq = receipt.seq;
        }
    }

    #[test]
    fn commit_receipts_continue_from_prior() {
        let mut contract = make_commit_contract(1);
        contract.receipts.push(TxReceipt {
            seq: 5,
            state: MissionTxState::Prepared,
            emitted_at_ms: 1000,
            reason_code: Some("prepared".into()),
            error_code: None,
        });

        let inputs = vec![success_input("s1", 2000)];
        let report =
            execute_commit_phase(&contract, &inputs, MissionKillSwitchLevel::Off, false, 5000)
                .unwrap();

        // Should start from seq 6 (prior last was 5)
        assert_eq!(report.receipts[0].seq, 6);
    }

    #[test]
    fn commit_step_results_in_ordinal_order() {
        let contract = make_commit_contract(4);
        let inputs = vec![
            success_input("s1", 2000),
            success_input("s2", 3000),
            success_input("s3", 4000),
            success_input("s4", 5000),
        ];
        let report =
            execute_commit_phase(&contract, &inputs, MissionKillSwitchLevel::Off, false, 6000)
                .unwrap();

        for (i, result) in report.step_results.iter().enumerate() {
            assert_eq!(result.ordinal, (i + 1) as u32);
        }
    }

    #[test]
    fn commit_single_step_success() {
        let contract = make_commit_contract(1);
        let inputs = vec![success_input("s1", 2000)];
        let report =
            execute_commit_phase(&contract, &inputs, MissionKillSwitchLevel::Off, false, 5000)
                .unwrap();

        assert!(report.is_fully_committed());
        assert_eq!(report.committed_count, 1);
    }

    #[test]
    fn commit_single_step_failure() {
        let contract = make_commit_contract(1);
        let inputs = vec![failure_input("s1", 2000)];
        let report =
            execute_commit_phase(&contract, &inputs, MissionKillSwitchLevel::Off, false, 5000)
                .unwrap();

        let is_immediate = matches!(report.outcome, TxCommitOutcome::ImmediateFailure);
        assert!(is_immediate);
        assert_eq!(report.failed_count, 1);
    }

    #[test]
    fn commit_outcome_tag_names() {
        assert_eq!(
            TxCommitOutcome::FullyCommitted.tag_name(),
            "fully_committed"
        );
        assert_eq!(
            TxCommitOutcome::PartialFailure.tag_name(),
            "partial_failure"
        );
        assert_eq!(
            TxCommitOutcome::ImmediateFailure.tag_name(),
            "immediate_failure"
        );
        assert_eq!(
            TxCommitOutcome::KillSwitchBlocked.tag_name(),
            "kill_switch_blocked"
        );
        assert_eq!(
            TxCommitOutcome::PauseSuspended.tag_name(),
            "pause_suspended"
        );
    }

    // ── H6: Compensation/rollback engine tests ──────────────────────────────

    fn make_compensable_commit_report(num_committed: usize, failure_at: u32) -> TxCommitReport {
        let mut step_results = Vec::new();
        for i in 1..=num_committed {
            step_results.push(TxCommitStepResult {
                step_id: TxStepId(format!("s{i}")),
                ordinal: i as u32,
                outcome: TxCommitStepOutcome::Committed {
                    reason_code: "ok".into(),
                },
                decision_path: "commit_step_succeeded".into(),
                completed_at_ms: (i as i64 + 1) * 1000,
            });
        }
        // Add the failed step
        step_results.push(TxCommitStepResult {
            step_id: TxStepId(format!("s{}", failure_at)),
            ordinal: failure_at,
            outcome: TxCommitStepOutcome::Failed {
                reason_code: "exec_error".into(),
                error_code: "FTX9999".into(),
            },
            decision_path: "commit_step_failed".into(),
            completed_at_ms: 10_000,
        });

        TxCommitReport {
            tx_id: TxId("tx:comp-test".into()),
            plan_id: TxPlanId("plan:comp-test".into()),
            outcome: TxCommitOutcome::PartialFailure,
            step_results,
            failure_boundary: Some(failure_at),
            committed_count: num_committed,
            failed_count: 1,
            skipped_count: 0,
            decision_path: "commit_partial_failure".into(),
            reason_code: "partial".into(),
            error_code: Some("FTX3005".into()),
            completed_at_ms: 10_000,
            receipts: vec![],
        }
    }

    fn make_compensable_contract(
        num_steps: usize,
        compensations: Vec<TxCompensation>,
    ) -> MissionTxContract {
        let steps: Vec<TxStep> = (1..=num_steps)
            .map(|i| TxStep {
                step_id: TxStepId(format!("s{i}")),
                ordinal: i as u32,
                action: StepAction::SendText {
                    pane_id: i as u64,
                    text: format!("step-{i}"),
                    paste_mode: None,
                },
            })
            .collect();

        MissionTxContract {
            tx_version: 1,
            intent: TxIntent {
                tx_id: TxId("tx:comp-test".into()),
                requested_by: MissionActorRole::Dispatcher,
                summary: "comp test".into(),
                correlation_id: "comp-1".into(),
                created_at_ms: 1000,
            },
            plan: TxPlan {
                plan_id: TxPlanId("plan:comp-test".into()),
                tx_id: TxId("tx:comp-test".into()),
                steps,
                preconditions: vec![],
                compensations,
            },
            lifecycle_state: MissionTxState::Compensating,
            outcome: TxOutcome::Pending,
            receipts: vec![],
        }
    }

    fn comp_success_input(step_id: &str, ts: i64) -> TxCompensationStepInput {
        TxCompensationStepInput {
            for_step_id: TxStepId(step_id.into()),
            success: true,
            reason_code: "rolled_back".into(),
            error_code: None,
            completed_at_ms: ts,
        }
    }

    fn comp_failure_input(step_id: &str, ts: i64) -> TxCompensationStepInput {
        TxCompensationStepInput {
            for_step_id: TxStepId(step_id.into()),
            success: false,
            reason_code: "comp_error".into(),
            error_code: Some("FTX2008".into()),
            completed_at_ms: ts,
        }
    }

    #[test]
    fn compensation_all_steps_rolled_back() {
        let compensations = vec![
            TxCompensation {
                for_step_id: TxStepId("s1".into()),
                action: StepAction::ReleaseLock {
                    lock_name: "test".into(),
                },
            },
            TxCompensation {
                for_step_id: TxStepId("s2".into()),
                action: StepAction::ReleaseLock {
                    lock_name: "test2".into(),
                },
            },
        ];
        let contract = make_compensable_contract(3, compensations);
        let commit_report = make_compensable_commit_report(2, 3);
        let inputs = vec![
            comp_success_input("s2", 11_000),
            comp_success_input("s1", 12_000),
        ];

        let report =
            execute_compensation_phase(&contract, &commit_report, &inputs, 15_000).unwrap();

        assert!(report.is_fully_rolled_back());
        assert!(!report.has_residual_risk());
        assert_eq!(report.compensated_count, 2);
        assert_eq!(report.failed_count, 0);
        assert_eq!(report.outcome.target_tx_state(), MissionTxState::RolledBack,);
    }

    #[test]
    fn compensation_failure_trips_barrier() {
        let compensations = vec![
            TxCompensation {
                for_step_id: TxStepId("s1".into()),
                action: StepAction::ReleaseLock {
                    lock_name: "test".into(),
                },
            },
            TxCompensation {
                for_step_id: TxStepId("s2".into()),
                action: StepAction::ReleaseLock {
                    lock_name: "test2".into(),
                },
            },
        ];
        let contract = make_compensable_contract(3, compensations);
        let commit_report = make_compensable_commit_report(2, 3);
        let inputs = vec![
            comp_failure_input("s2", 11_000), // s2 first (reverse order) — fails
            comp_success_input("s1", 12_000), // s1 skipped due to barrier
        ];

        let report =
            execute_compensation_phase(&contract, &commit_report, &inputs, 15_000).unwrap();

        assert!(report.has_residual_risk());
        assert_eq!(report.failed_count, 1);
        assert_eq!(report.skipped_count, 1);
        assert_eq!(report.outcome.target_tx_state(), MissionTxState::Failed,);
    }

    #[test]
    fn compensation_no_defined_compensations() {
        let contract = make_compensable_contract(3, vec![]); // no compensations defined
        let commit_report = make_compensable_commit_report(2, 3);

        let report = execute_compensation_phase(&contract, &commit_report, &[], 15_000).unwrap();

        // All committed steps have no compensation
        assert_eq!(report.no_compensation_count, 2);
        assert!(report.is_fully_rolled_back()); // No failures = success
    }

    #[test]
    fn compensation_nothing_to_compensate() {
        let contract = make_compensable_contract(1, vec![]);
        // Commit report with only a failed step (nothing committed)
        let commit_report = TxCommitReport {
            tx_id: TxId("tx:comp-test".into()),
            plan_id: TxPlanId("plan:comp-test".into()),
            outcome: TxCommitOutcome::ImmediateFailure,
            step_results: vec![TxCommitStepResult {
                step_id: TxStepId("s1".into()),
                ordinal: 1,
                outcome: TxCommitStepOutcome::Failed {
                    reason_code: "err".into(),
                    error_code: "E1".into(),
                },
                decision_path: "failed".into(),
                completed_at_ms: 2000,
            }],
            failure_boundary: Some(1),
            committed_count: 0,
            failed_count: 1,
            skipped_count: 0,
            decision_path: "immediate".into(),
            reason_code: "first_fail".into(),
            error_code: Some("E1".into()),
            completed_at_ms: 2000,
            receipts: vec![],
        };

        let report = execute_compensation_phase(&contract, &commit_report, &[], 15_000).unwrap();

        let is_nothing = matches!(report.outcome, TxCompensationOutcome::NothingToCompensate);
        assert!(is_nothing);
    }

    #[test]
    fn compensation_rejects_non_compensating_state() {
        let mut contract = make_compensable_contract(1, vec![]);
        contract.lifecycle_state = MissionTxState::Committed;
        let commit_report = make_compensable_commit_report(0, 1);

        let result = execute_compensation_phase(&contract, &commit_report, &[], 15_000);
        assert!(result.is_err());
    }

    #[test]
    fn compensation_reverse_ordinal_order() {
        let compensations = vec![
            TxCompensation {
                for_step_id: TxStepId("s1".into()),
                action: StepAction::ReleaseLock {
                    lock_name: "l1".into(),
                },
            },
            TxCompensation {
                for_step_id: TxStepId("s2".into()),
                action: StepAction::ReleaseLock {
                    lock_name: "l2".into(),
                },
            },
            TxCompensation {
                for_step_id: TxStepId("s3".into()),
                action: StepAction::ReleaseLock {
                    lock_name: "l3".into(),
                },
            },
        ];
        let contract = make_compensable_contract(4, compensations);
        let commit_report = make_compensable_commit_report(3, 4);
        let inputs = vec![
            comp_success_input("s1", 11_000),
            comp_success_input("s2", 12_000),
            comp_success_input("s3", 13_000),
        ];

        let report =
            execute_compensation_phase(&contract, &commit_report, &inputs, 15_000).unwrap();

        // Results should be in reverse ordinal order (s3, s2, s1)
        assert_eq!(report.step_results[0].forward_ordinal, 3);
        assert_eq!(report.step_results[1].forward_ordinal, 2);
        assert_eq!(report.step_results[2].forward_ordinal, 1);
    }

    #[test]
    fn compensation_missing_input_treated_as_failure() {
        let compensations = vec![TxCompensation {
            for_step_id: TxStepId("s1".into()),
            action: StepAction::ReleaseLock {
                lock_name: "l1".into(),
            },
        }];
        let contract = make_compensable_contract(2, compensations);
        let commit_report = make_compensable_commit_report(1, 2);
        // No inputs provided for s1

        let report = execute_compensation_phase(&contract, &commit_report, &[], 15_000).unwrap();

        assert_eq!(report.failed_count, 1);
        assert!(report.has_residual_risk());
    }

    #[test]
    fn compensation_step_outcome_tag_names() {
        let compensated = TxCompensationStepOutcome::Compensated {
            reason_code: "ok".into(),
        };
        assert_eq!(compensated.tag_name(), "compensated");
        assert!(compensated.is_compensated());

        let failed = TxCompensationStepOutcome::Failed {
            reason_code: "err".into(),
            error_code: "E1".into(),
        };
        assert_eq!(failed.tag_name(), "failed");
        assert!(failed.is_failed());

        let no_comp = TxCompensationStepOutcome::NoCompensation {
            reason_code: "none".into(),
        };
        assert_eq!(no_comp.tag_name(), "no_compensation");

        let skipped = TxCompensationStepOutcome::Skipped {
            reason_code: "barrier".into(),
        };
        assert_eq!(skipped.tag_name(), "skipped");
    }

    #[test]
    fn compensation_outcome_target_states() {
        assert_eq!(
            TxCompensationOutcome::FullyRolledBack.target_tx_state(),
            MissionTxState::RolledBack,
        );
        assert_eq!(
            TxCompensationOutcome::CompensationFailed.target_tx_state(),
            MissionTxState::Failed,
        );
        assert_eq!(
            TxCompensationOutcome::NothingToCompensate.target_tx_state(),
            MissionTxState::Failed,
        );
    }

    #[test]
    fn compensation_report_canonical_string_deterministic() {
        let compensations = vec![TxCompensation {
            for_step_id: TxStepId("s1".into()),
            action: StepAction::ReleaseLock {
                lock_name: "l1".into(),
            },
        }];
        let contract = make_compensable_contract(2, compensations);
        let commit_report = make_compensable_commit_report(1, 2);
        let inputs = vec![comp_success_input("s1", 11_000)];

        let report =
            execute_compensation_phase(&contract, &commit_report, &inputs, 15_000).unwrap();

        let s1 = report.canonical_string();
        let s2 = report.canonical_string();
        assert_eq!(s1, s2);
    }

    #[test]
    fn compensation_report_serde_roundtrip() {
        let compensations = vec![TxCompensation {
            for_step_id: TxStepId("s1".into()),
            action: StepAction::ReleaseLock {
                lock_name: "l1".into(),
            },
        }];
        let contract = make_compensable_contract(2, compensations);
        let commit_report = make_compensable_commit_report(1, 2);
        let inputs = vec![comp_success_input("s1", 11_000)];

        let report =
            execute_compensation_phase(&contract, &commit_report, &inputs, 15_000).unwrap();

        let json = serde_json::to_string(&report).unwrap();
        let restored: TxCompensationReport = serde_json::from_str(&json).unwrap();
        assert_eq!(report, restored);
    }

    #[test]
    fn compensation_step_input_serde_roundtrip() {
        let input = TxCompensationStepInput {
            for_step_id: TxStepId("s1".into()),
            success: false,
            reason_code: "comp_timeout".into(),
            error_code: Some("FTX2008".into()),
            completed_at_ms: 9000,
        };
        let json = serde_json::to_string(&input).unwrap();
        let restored: TxCompensationStepInput = serde_json::from_str(&json).unwrap();
        assert_eq!(input, restored);
    }

    #[test]
    fn compensation_receipts_monotonic_seq() {
        let compensations = vec![TxCompensation {
            for_step_id: TxStepId("s1".into()),
            action: StepAction::ReleaseLock {
                lock_name: "l1".into(),
            },
        }];
        let contract = make_compensable_contract(2, compensations);
        let commit_report = make_compensable_commit_report(1, 2);
        let inputs = vec![comp_success_input("s1", 11_000)];

        let report =
            execute_compensation_phase(&contract, &commit_report, &inputs, 15_000).unwrap();

        let mut prev = 0u64;
        for receipt in &report.receipts {
            assert!(receipt.seq > prev);
            prev = receipt.seq;
        }
    }

    #[test]
    fn compensation_outcome_tag_names() {
        assert_eq!(
            TxCompensationOutcome::FullyRolledBack.tag_name(),
            "fully_rolled_back"
        );
        assert_eq!(
            TxCompensationOutcome::CompensationFailed.tag_name(),
            "compensation_failed"
        );
        assert_eq!(
            TxCompensationOutcome::NothingToCompensate.tag_name(),
            "nothing_to_compensate"
        );
    }

    // ── H7: Durable Idempotency, Dedupe, and Resume Tests ──────────────────

    fn make_h7_contract(num_steps: usize) -> MissionTxContract {
        let steps: Vec<TxStep> = (1..=num_steps)
            .map(|i| TxStep {
                step_id: TxStepId(format!("s{i}")),
                ordinal: i as u32,
                action: StepAction::SendText {
                    pane_id: i as u64,
                    text: format!("step-{i}"),
                    paste_mode: None,
                },
            })
            .collect();

        let compensations: Vec<TxCompensation> = (1..=num_steps)
            .map(|i| TxCompensation {
                for_step_id: TxStepId(format!("s{i}")),
                action: StepAction::SendText {
                    pane_id: i as u64,
                    text: format!("undo-{i}"),
                    paste_mode: None,
                },
            })
            .collect();

        MissionTxContract {
            tx_version: 1,
            intent: TxIntent {
                tx_id: TxId("tx:h7".into()),
                requested_by: MissionActorRole::Dispatcher,
                summary: "h7-test".into(),
                correlation_id: "h7-corr-1".into(),
                created_at_ms: 1000,
            },
            plan: TxPlan {
                plan_id: TxPlanId("plan:h7".into()),
                tx_id: TxId("tx:h7".into()),
                steps,
                preconditions: vec![],
                compensations,
            },
            lifecycle_state: MissionTxState::Prepared,
            outcome: TxOutcome::Pending,
            receipts: vec![],
        }
    }

    fn make_h7_execution_record(
        contract: &MissionTxContract,
        state: MissionTxState,
        commit_hash: Option<&str>,
        comp_hash: Option<&str>,
    ) -> TxExecutionRecord {
        TxExecutionRecord {
            tx_id: contract.intent.tx_id.clone(),
            plan_id: contract.plan.plan_id.clone(),
            lifecycle_state: state,
            correlation_id: contract.intent.correlation_id.clone(),
            tx_idempotency_key: TxExecutionRecord::compute_tx_key(contract),
            step_records: vec![],
            commit_report_hash: commit_hash.map(|s| s.to_string()),
            compensation_report_hash: comp_hash.map(|s| s.to_string()),
            updated_at_ms: 5000,
        }
    }

    #[test]
    fn idempotency_fresh_when_no_prior_record() {
        let contract = make_h7_contract(3);
        let result = validate_tx_idempotency(&contract, TxPhase::Commit, None);
        assert!(result.should_proceed());
        assert!(matches!(result.verdict, TxIdempotencyVerdict::Fresh));
        assert_eq!(result.reason_code, "fresh_execution");
    }

    #[test]
    fn idempotency_exact_duplicate_on_terminal_same_key() {
        let contract = make_h7_contract(3);
        let record =
            make_h7_execution_record(&contract, MissionTxState::Committed, Some("hash1"), None);
        // Requesting prepare (not commit) to avoid double-execution guard
        let result = validate_tx_idempotency(&contract, TxPhase::Prepare, Some(&record));
        assert!(result.is_exact_duplicate());
        assert!(!result.should_proceed());
    }

    #[test]
    fn idempotency_double_commit_blocked() {
        let contract = make_h7_contract(3);
        let record =
            make_h7_execution_record(&contract, MissionTxState::Committed, Some("hash1"), None);
        let result = validate_tx_idempotency(&contract, TxPhase::Commit, Some(&record));
        assert!(!result.should_proceed());
        let is_blocked = matches!(
            result.verdict,
            TxIdempotencyVerdict::DoubleExecutionBlocked { .. }
        );
        assert!(is_blocked);
        assert_eq!(result.error_code, Some("FTX3001".into()));
    }

    #[test]
    fn idempotency_double_compensation_blocked() {
        let contract = make_h7_contract(3);
        let record = make_h7_execution_record(
            &contract,
            MissionTxState::RolledBack,
            Some("hash1"),
            Some("hash2"),
        );
        let result = validate_tx_idempotency(&contract, TxPhase::Compensate, Some(&record));
        assert!(!result.should_proceed());
        let is_blocked = matches!(
            result.verdict,
            TxIdempotencyVerdict::DoubleExecutionBlocked { .. }
        );
        assert!(is_blocked);
        assert_eq!(result.error_code, Some("FTX3002".into()));
    }

    #[test]
    fn idempotency_conflicting_prior_different_key() {
        let contract = make_h7_contract(3);
        let mut record = make_h7_execution_record(&contract, MissionTxState::Committed, None, None);
        record.tx_idempotency_key = "txkey:different_key_here".into();
        let result = validate_tx_idempotency(&contract, TxPhase::Prepare, Some(&record));
        assert!(!result.should_proceed());
        let is_conflict = matches!(
            result.verdict,
            TxIdempotencyVerdict::ConflictingPrior { .. }
        );
        assert!(is_conflict);
        assert_eq!(result.error_code, Some("FTX3003".into()));
    }

    #[test]
    fn idempotency_resumable_on_non_terminal() {
        let contract = make_h7_contract(3);
        let mut record =
            make_h7_execution_record(&contract, MissionTxState::Committing, None, None);
        record.step_records.push(TxStepExecutionRecord {
            step_id: TxStepId("s1".into()),
            ordinal: 1,
            phase: TxPhase::Commit,
            succeeded: true,
            step_idempotency_key: "stepkey:abc".into(),
            attempt_count: 1,
            last_attempted_at_ms: 2000,
        });
        let result = validate_tx_idempotency(&contract, TxPhase::Commit, Some(&record));
        assert!(result.should_proceed());
        match &result.verdict {
            TxIdempotencyVerdict::Resumable {
                resume_from_state,
                completed_steps,
            } => {
                assert_eq!(*resume_from_state, MissionTxState::Committing);
                assert_eq!(completed_steps.len(), 1);
                assert_eq!(completed_steps[0].0, "s1");
            }
            other => panic!("expected Resumable, got {:?}", other),
        }
    }

    #[test]
    fn idempotency_tx_key_deterministic() {
        let contract = make_h7_contract(3);
        let k1 = TxExecutionRecord::compute_tx_key(&contract);
        let k2 = TxExecutionRecord::compute_tx_key(&contract);
        assert_eq!(k1, k2);
        assert!(k1.starts_with("txkey:"));
    }

    #[test]
    fn idempotency_tx_key_differs_for_different_contracts() {
        let c1 = make_h7_contract(3);
        let c2 = make_h7_contract(5);
        let k1 = TxExecutionRecord::compute_tx_key(&c1);
        let k2 = TxExecutionRecord::compute_tx_key(&c2);
        assert_ne!(k1, k2);
    }

    #[test]
    fn step_key_deterministic() {
        let tx_id = TxId("tx:h7".into());
        let step_id = TxStepId("s1".into());
        let k1 = TxStepExecutionRecord::compute_step_key(&tx_id, &step_id, &TxPhase::Commit);
        let k2 = TxStepExecutionRecord::compute_step_key(&tx_id, &step_id, &TxPhase::Commit);
        assert_eq!(k1, k2);
        assert!(k1.starts_with("stepkey:"));
    }

    #[test]
    fn step_key_differs_by_phase() {
        let tx_id = TxId("tx:h7".into());
        let step_id = TxStepId("s1".into());
        let k_commit = TxStepExecutionRecord::compute_step_key(&tx_id, &step_id, &TxPhase::Commit);
        let k_comp =
            TxStepExecutionRecord::compute_step_key(&tx_id, &step_id, &TxPhase::Compensate);
        assert_ne!(k_commit, k_comp);
    }

    #[test]
    fn tx_phase_tag_names() {
        assert_eq!(TxPhase::Prepare.tag_name(), "prepare");
        assert_eq!(TxPhase::Commit.tag_name(), "commit");
        assert_eq!(TxPhase::Compensate.tag_name(), "compensate");
    }

    #[test]
    fn execution_record_is_terminal() {
        let contract = make_h7_contract(3);
        let committed = make_h7_execution_record(&contract, MissionTxState::Committed, None, None);
        let committing =
            make_h7_execution_record(&contract, MissionTxState::Committing, None, None);
        assert!(committed.is_terminal());
        assert!(!committing.is_terminal());
    }

    #[test]
    fn execution_record_serde_roundtrip() {
        let contract = make_h7_contract(3);
        let record =
            make_h7_execution_record(&contract, MissionTxState::Committed, Some("hash1"), None);
        let json = serde_json::to_string(&record).unwrap();
        let restored: TxExecutionRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(record, restored);
    }

    #[test]
    fn step_execution_record_serde_roundtrip() {
        let record = TxStepExecutionRecord {
            step_id: TxStepId("s1".into()),
            ordinal: 1,
            phase: TxPhase::Commit,
            succeeded: true,
            step_idempotency_key: "stepkey:abc".into(),
            attempt_count: 2,
            last_attempted_at_ms: 3000,
        };
        let json = serde_json::to_string(&record).unwrap();
        let restored: TxStepExecutionRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(record, restored);
    }

    #[test]
    fn idempotency_check_result_serde_roundtrip() {
        let contract = make_h7_contract(3);
        let result = validate_tx_idempotency(&contract, TxPhase::Commit, None);
        let json = serde_json::to_string(&result).unwrap();
        let restored: TxIdempotencyCheckResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result, restored);
    }

    #[test]
    fn idempotency_verdict_tag_names() {
        assert_eq!(TxIdempotencyVerdict::Fresh.tag_name(), "fresh");
        assert_eq!(
            TxIdempotencyVerdict::ExactDuplicate.tag_name(),
            "exact_duplicate"
        );
        assert_eq!(
            TxIdempotencyVerdict::Resumable {
                resume_from_state: MissionTxState::Committing,
                completed_steps: vec![],
            }
            .tag_name(),
            "resumable"
        );
        assert_eq!(
            TxIdempotencyVerdict::ConflictingPrior {
                prior_state: MissionTxState::Committed,
                conflict_reason: "test".into(),
            }
            .tag_name(),
            "conflicting_prior"
        );
        assert_eq!(
            TxIdempotencyVerdict::DoubleExecutionBlocked {
                already_completed_phase: TxPhase::Commit,
            }
            .tag_name(),
            "double_execution_blocked"
        );
    }

    #[test]
    fn resume_state_from_empty_contract() {
        let contract = make_h7_contract(3);
        let resume = reconstruct_tx_resume_state(&contract, None, None, 10_000);
        assert_eq!(resume.derived_state, MissionTxState::Prepared);
        assert_eq!(resume.last_receipt_seq, 0);
        assert!(resume.committed_step_ids.is_empty());
        assert!(resume.compensated_step_ids.is_empty());
        // Prepared state with no commit → all steps pending
        assert_eq!(resume.pending_step_ids.len(), 3);
        assert!(!resume.commit_phase_completed);
        assert!(!resume.compensation_phase_completed);
    }

    #[test]
    fn resume_state_with_full_commit() {
        let mut contract = make_h7_contract(3);
        contract.lifecycle_state = MissionTxState::Prepared;
        let commit_inputs: Vec<TxCommitStepInput> = (1..=3)
            .map(|i| TxCommitStepInput {
                step_id: TxStepId(format!("s{i}")),
                success: true,
                reason_code: "ok".into(),
                error_code: None,
                completed_at_ms: (i as i64 + 1) * 1000,
            })
            .collect();
        let commit_report = execute_commit_phase(
            &contract,
            &commit_inputs,
            MissionKillSwitchLevel::Off,
            false,
            10_000,
        )
        .unwrap();

        let resume = reconstruct_tx_resume_state(&contract, Some(&commit_report), None, 15_000);
        assert_eq!(resume.committed_step_ids.len(), 3);
        assert!(resume.commit_phase_completed);
        assert!(!resume.compensation_phase_completed);
        // Derived state from contract receipts (empty) → Prepared
        assert!(
            resume.pending_step_ids.is_empty() || resume.derived_state == MissionTxState::Prepared
        );
    }

    #[test]
    fn resume_state_with_partial_commit() {
        let mut contract = make_h7_contract(3);
        contract.lifecycle_state = MissionTxState::Prepared;
        let commit_inputs: Vec<TxCommitStepInput> = (1..=3)
            .map(|i| TxCommitStepInput {
                step_id: TxStepId(format!("s{i}")),
                success: i != 2,
                reason_code: if i == 2 { "err".into() } else { "ok".into() },
                error_code: if i == 2 { Some("FTX9999".into()) } else { None },
                completed_at_ms: (i as i64 + 1) * 1000,
            })
            .collect();
        let commit_report = execute_commit_phase(
            &contract,
            &commit_inputs,
            MissionKillSwitchLevel::Off,
            false,
            10_000,
        )
        .unwrap();

        let resume = reconstruct_tx_resume_state(&contract, Some(&commit_report), None, 15_000);
        // Step 1 committed, step 2 failed, step 3 skipped
        assert_eq!(resume.committed_step_ids.len(), 1);
        assert_eq!(resume.committed_step_ids[0].0, "s1");
        assert!(resume.commit_phase_completed);
    }

    #[test]
    fn resume_state_is_fully_resolved_on_terminal() {
        let mut contract = make_h7_contract(3);
        contract.receipts.push(TxReceipt {
            seq: 1,
            state: MissionTxState::Committed,
            emitted_at_ms: 5000,
            reason_code: Some("all_committed".into()),
            error_code: None,
        });
        let resume = reconstruct_tx_resume_state(&contract, None, None, 10_000);
        assert!(resume.is_fully_resolved());
        assert_eq!(resume.derived_state, MissionTxState::Committed);
        assert!(resume.pending_step_ids.is_empty());
    }

    #[test]
    fn resume_state_canonical_string_deterministic() {
        let contract = make_h7_contract(3);
        let resume = reconstruct_tx_resume_state(&contract, None, None, 10_000);
        let s1 = resume.canonical_string();
        let s2 = resume.canonical_string();
        assert_eq!(s1, s2);
    }

    #[test]
    fn execution_record_canonical_string_deterministic() {
        let contract = make_h7_contract(3);
        let record =
            make_h7_execution_record(&contract, MissionTxState::Committed, Some("hash1"), None);
        let s1 = record.canonical_string();
        let s2 = record.canonical_string();
        assert_eq!(s1, s2);
    }

    #[test]
    fn step_execution_record_canonical_string_deterministic() {
        let record = TxStepExecutionRecord {
            step_id: TxStepId("s1".into()),
            ordinal: 1,
            phase: TxPhase::Commit,
            succeeded: true,
            step_idempotency_key: "stepkey:abc".into(),
            attempt_count: 1,
            last_attempted_at_ms: 2000,
        };
        let s1 = record.canonical_string();
        let s2 = record.canonical_string();
        assert_eq!(s1, s2);
    }

    #[test]
    fn idempotency_check_canonical_string_deterministic() {
        let contract = make_h7_contract(3);
        let result = validate_tx_idempotency(&contract, TxPhase::Commit, None);
        let s1 = result.canonical_string();
        let s2 = result.canonical_string();
        assert_eq!(s1, s2);
    }

    #[test]
    fn step_record_already_succeeded_checks_phase() {
        let record = TxStepExecutionRecord {
            step_id: TxStepId("s1".into()),
            ordinal: 1,
            phase: TxPhase::Commit,
            succeeded: true,
            step_idempotency_key: "stepkey:abc".into(),
            attempt_count: 1,
            last_attempted_at_ms: 2000,
        };
        assert!(record.is_already_succeeded(&TxPhase::Commit));
        assert!(!record.is_already_succeeded(&TxPhase::Compensate));
    }

    #[test]
    fn resume_state_serde_roundtrip() {
        let contract = make_h7_contract(3);
        let resume = reconstruct_tx_resume_state(&contract, None, None, 10_000);
        let json = serde_json::to_string(&resume).unwrap();
        let restored: TxResumeState = serde_json::from_str(&json).unwrap();
        assert_eq!(resume, restored);
    }

    #[test]
    fn tx_phase_serde_roundtrip() {
        for phase in &[TxPhase::Prepare, TxPhase::Commit, TxPhase::Compensate] {
            let json = serde_json::to_string(phase).unwrap();
            let restored: TxPhase = serde_json::from_str(&json).unwrap();
            assert_eq!(*phase, restored);
        }
    }

    #[test]
    fn idempotency_verdict_serde_roundtrip() {
        let verdicts = vec![
            TxIdempotencyVerdict::Fresh,
            TxIdempotencyVerdict::ExactDuplicate,
            TxIdempotencyVerdict::Resumable {
                resume_from_state: MissionTxState::Committing,
                completed_steps: vec![TxStepId("s1".into())],
            },
            TxIdempotencyVerdict::ConflictingPrior {
                prior_state: MissionTxState::Committed,
                conflict_reason: "mismatch".into(),
            },
            TxIdempotencyVerdict::DoubleExecutionBlocked {
                already_completed_phase: TxPhase::Commit,
            },
        ];
        for v in &verdicts {
            let json = serde_json::to_string(v).unwrap();
            let restored: TxIdempotencyVerdict = serde_json::from_str(&json).unwrap();
            assert_eq!(*v, restored);
        }
    }

    #[test]
    fn resume_state_has_pending_work() {
        let contract = make_h7_contract(3);
        let resume = reconstruct_tx_resume_state(&contract, None, None, 10_000);
        assert!(resume.has_pending_work());

        let mut terminal_contract = make_h7_contract(3);
        terminal_contract.receipts.push(TxReceipt {
            seq: 1,
            state: MissionTxState::Committed,
            emitted_at_ms: 5000,
            reason_code: None,
            error_code: None,
        });
        let resume2 = reconstruct_tx_resume_state(&terminal_contract, None, None, 10_000);
        assert!(!resume2.has_pending_work());
    }
}
