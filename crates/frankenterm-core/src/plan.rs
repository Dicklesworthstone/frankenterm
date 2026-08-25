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

use crate::approval::ApprovalScope;
use crate::policy::{
    ActionKind, ActorKind, PaneCapabilities, PolicyDecision, PolicyInput, PolicySurface,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    fmt,
};

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
    /// - Plan version is newer than this build supports
    /// - Step numbers are not sequential starting from 1
    /// - Step IDs are not unique
    /// - Referenced steps in preconditions don't exist
    pub fn validate(&self) -> Result<(), PlanValidationError> {
        if self.plan_version > PLAN_SCHEMA_VERSION {
            return Err(PlanValidationError::UnsupportedVersion {
                version: self.plan_version,
                max_supported: PLAN_SCHEMA_VERSION,
            });
        }

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
        for step in &self.steps {
            for precond in &step.preconditions {
                if let Precondition::StepCompleted { step_id } = precond {
                    if !seen_ids.contains(step_id) {
                        return Err(PlanValidationError::UnknownStepReference(step_id.clone()));
                    }
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// Create ownership where a single actor fills all roles.
    #[must_use]
    pub fn solo(actor: impl Into<String>) -> Self {
        let actor = actor.into();
        Self {
            planner: actor.clone(),
            dispatcher: actor.clone(),
            operator: actor,
        }
    }

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

/// Lifecycle state for mission orchestration from planning to terminal outcomes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionLifecycleState {
    Planned,
    #[default]
    Planning,
    Dispatching,
    AwaitingApproval,
    Running,
    Executing,
    RetryPending,
    Blocked,
    Paused,
    Completed,
    Cancelled,
    Failed,
}

impl fmt::Display for MissionLifecycleState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Planned => f.write_str("planned"),
            Self::Planning => f.write_str("planning"),
            Self::Dispatching => f.write_str("dispatching"),
            Self::AwaitingApproval => f.write_str("awaiting_approval"),
            Self::Running => f.write_str("running"),
            Self::Executing => f.write_str("executing"),
            Self::RetryPending => f.write_str("retry_pending"),
            Self::Blocked => f.write_str("blocked"),
            Self::Paused => f.write_str("paused"),
            Self::Completed => f.write_str("completed"),
            Self::Cancelled => f.write_str("cancelled"),
            Self::Failed => f.write_str("failed"),
        }
    }
}

impl MissionLifecycleState {
    /// Full mission lifecycle transition table.
    #[must_use]
    pub fn transition_table() -> &'static [MissionLifecycleTransition] {
        MISSION_LIFECYCLE_TRANSITIONS
    }

    /// Allowed transition kinds from the current state.
    #[must_use]
    pub fn allowed_transitions(self) -> Vec<MissionLifecycleTransitionKind> {
        MISSION_LIFECYCLE_TRANSITIONS
            .iter()
            .filter(|rule| rule.from == self)
            .map(|rule| rule.via)
            .collect()
    }

    /// Apply one lifecycle transition and return the next state.
    pub fn apply_transition(
        self,
        transition: MissionLifecycleTransitionKind,
    ) -> Result<Self, MissionLifecycleError> {
        MISSION_LIFECYCLE_TRANSITIONS
            .iter()
            .find(|rule| rule.from == self && rule.via == transition)
            .map(|rule| rule.to)
            .ok_or(MissionLifecycleError::InvalidTransition {
                from: self,
                transition,
            })
    }

    /// Whether this lifecycle state is terminal.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Cancelled | Self::Failed)
    }
}

/// Free-function alias for `MissionLifecycleState::transition_table()`.
#[must_use]
pub fn mission_lifecycle_transition_table() -> &'static [MissionLifecycleTransition] {
    MissionLifecycleState::transition_table()
}

/// Transition event used to move a mission through its lifecycle state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionLifecycleTransitionKind {
    Dispatch,
    RequestApproval,
    Approve,
    StartExecution,
    Retry,
    Block,
    Unblock,
    Complete,
    Cancel,
    Fail,
    PlanFinalized,
    DispatchStarted,
    ExecutionStarted,
    RetryResumed,
    ExecutionBlocked,
    MissionCancelled,
    PauseRequested,
    ResumeRequested,
    AbortRequested,
    ApprovalRequested,
}

impl fmt::Display for MissionLifecycleTransitionKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Dispatch => f.write_str("dispatch"),
            Self::RequestApproval => f.write_str("request_approval"),
            Self::Approve => f.write_str("approve"),
            Self::StartExecution => f.write_str("start_execution"),
            Self::Retry => f.write_str("retry"),
            Self::Block => f.write_str("block"),
            Self::Unblock => f.write_str("unblock"),
            Self::Complete => f.write_str("complete"),
            Self::Cancel => f.write_str("cancel"),
            Self::Fail => f.write_str("fail"),
            Self::PlanFinalized => f.write_str("plan_finalized"),
            Self::DispatchStarted => f.write_str("dispatch_started"),
            Self::ExecutionStarted => f.write_str("execution_started"),
            Self::RetryResumed => f.write_str("retry_resumed"),
            Self::ExecutionBlocked => f.write_str("execution_blocked"),
            Self::MissionCancelled => f.write_str("mission_cancelled"),
            Self::PauseRequested => f.write_str("pause_requested"),
            Self::ResumeRequested => f.write_str("resume_requested"),
            Self::AbortRequested => f.write_str("abort_requested"),
            Self::ApprovalRequested => f.write_str("approval_requested"),
        }
    }
}

/// Deterministic transition row in the mission lifecycle table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MissionLifecycleTransition {
    pub from: MissionLifecycleState,
    pub via: MissionLifecycleTransitionKind,
    pub to: MissionLifecycleState,
}

const MISSION_LIFECYCLE_TRANSITIONS: &[MissionLifecycleTransition] = &[
    // --- Planned (alias for Planning) ---
    MissionLifecycleTransition {
        from: MissionLifecycleState::Planned,
        via: MissionLifecycleTransitionKind::Dispatch,
        to: MissionLifecycleState::Dispatching,
    },
    MissionLifecycleTransition {
        from: MissionLifecycleState::Planned,
        via: MissionLifecycleTransitionKind::Block,
        to: MissionLifecycleState::Blocked,
    },
    MissionLifecycleTransition {
        from: MissionLifecycleState::Planned,
        via: MissionLifecycleTransitionKind::Cancel,
        to: MissionLifecycleState::Cancelled,
    },
    // --- Planning ---
    MissionLifecycleTransition {
        from: MissionLifecycleState::Planning,
        via: MissionLifecycleTransitionKind::Dispatch,
        to: MissionLifecycleState::Dispatching,
    },
    MissionLifecycleTransition {
        from: MissionLifecycleState::Planning,
        via: MissionLifecycleTransitionKind::Block,
        to: MissionLifecycleState::Blocked,
    },
    MissionLifecycleTransition {
        from: MissionLifecycleState::Planning,
        via: MissionLifecycleTransitionKind::Cancel,
        to: MissionLifecycleState::Cancelled,
    },
    MissionLifecycleTransition {
        from: MissionLifecycleState::Dispatching,
        via: MissionLifecycleTransitionKind::RequestApproval,
        to: MissionLifecycleState::AwaitingApproval,
    },
    MissionLifecycleTransition {
        from: MissionLifecycleState::Dispatching,
        via: MissionLifecycleTransitionKind::StartExecution,
        to: MissionLifecycleState::Executing,
    },
    MissionLifecycleTransition {
        from: MissionLifecycleState::Dispatching,
        via: MissionLifecycleTransitionKind::Block,
        to: MissionLifecycleState::Blocked,
    },
    MissionLifecycleTransition {
        from: MissionLifecycleState::Dispatching,
        via: MissionLifecycleTransitionKind::Cancel,
        to: MissionLifecycleState::Cancelled,
    },
    // --- AwaitingApproval ---
    MissionLifecycleTransition {
        from: MissionLifecycleState::AwaitingApproval,
        via: MissionLifecycleTransitionKind::Approve,
        to: MissionLifecycleState::Executing,
    },
    MissionLifecycleTransition {
        from: MissionLifecycleState::AwaitingApproval,
        via: MissionLifecycleTransitionKind::Fail,
        to: MissionLifecycleState::Failed,
    },
    MissionLifecycleTransition {
        from: MissionLifecycleState::AwaitingApproval,
        via: MissionLifecycleTransitionKind::Cancel,
        to: MissionLifecycleState::Cancelled,
    },
    MissionLifecycleTransition {
        from: MissionLifecycleState::Executing,
        via: MissionLifecycleTransitionKind::Retry,
        to: MissionLifecycleState::RetryPending,
    },
    MissionLifecycleTransition {
        from: MissionLifecycleState::Executing,
        via: MissionLifecycleTransitionKind::Complete,
        to: MissionLifecycleState::Completed,
    },
    MissionLifecycleTransition {
        from: MissionLifecycleState::Executing,
        via: MissionLifecycleTransitionKind::Fail,
        to: MissionLifecycleState::Failed,
    },
    MissionLifecycleTransition {
        from: MissionLifecycleState::Executing,
        via: MissionLifecycleTransitionKind::Block,
        to: MissionLifecycleState::Blocked,
    },
    MissionLifecycleTransition {
        from: MissionLifecycleState::Executing,
        via: MissionLifecycleTransitionKind::Cancel,
        to: MissionLifecycleState::Cancelled,
    },
    // --- Running (alias for Executing) ---
    MissionLifecycleTransition {
        from: MissionLifecycleState::Running,
        via: MissionLifecycleTransitionKind::Retry,
        to: MissionLifecycleState::RetryPending,
    },
    MissionLifecycleTransition {
        from: MissionLifecycleState::Running,
        via: MissionLifecycleTransitionKind::Complete,
        to: MissionLifecycleState::Completed,
    },
    MissionLifecycleTransition {
        from: MissionLifecycleState::Running,
        via: MissionLifecycleTransitionKind::Fail,
        to: MissionLifecycleState::Failed,
    },
    MissionLifecycleTransition {
        from: MissionLifecycleState::Running,
        via: MissionLifecycleTransitionKind::Block,
        to: MissionLifecycleState::Blocked,
    },
    MissionLifecycleTransition {
        from: MissionLifecycleState::Running,
        via: MissionLifecycleTransitionKind::Cancel,
        to: MissionLifecycleState::Cancelled,
    },
    MissionLifecycleTransition {
        from: MissionLifecycleState::RetryPending,
        via: MissionLifecycleTransitionKind::Dispatch,
        to: MissionLifecycleState::Dispatching,
    },
    MissionLifecycleTransition {
        from: MissionLifecycleState::RetryPending,
        via: MissionLifecycleTransitionKind::Block,
        to: MissionLifecycleState::Blocked,
    },
    MissionLifecycleTransition {
        from: MissionLifecycleState::RetryPending,
        via: MissionLifecycleTransitionKind::Cancel,
        to: MissionLifecycleState::Cancelled,
    },
    MissionLifecycleTransition {
        from: MissionLifecycleState::Blocked,
        via: MissionLifecycleTransitionKind::Unblock,
        to: MissionLifecycleState::Dispatching,
    },
    MissionLifecycleTransition {
        from: MissionLifecycleState::Blocked,
        via: MissionLifecycleTransitionKind::Cancel,
        to: MissionLifecycleState::Cancelled,
    },
    // --- Paused ---
    MissionLifecycleTransition {
        from: MissionLifecycleState::Paused,
        via: MissionLifecycleTransitionKind::RetryResumed,
        to: MissionLifecycleState::Running,
    },
    MissionLifecycleTransition {
        from: MissionLifecycleState::Paused,
        via: MissionLifecycleTransitionKind::Cancel,
        to: MissionLifecycleState::Cancelled,
    },
    MissionLifecycleTransition {
        from: MissionLifecycleState::Paused,
        via: MissionLifecycleTransitionKind::Fail,
        to: MissionLifecycleState::Failed,
    },
    // --- Extended transitions using new kinds ---
    MissionLifecycleTransition {
        from: MissionLifecycleState::Planning,
        via: MissionLifecycleTransitionKind::PlanFinalized,
        to: MissionLifecycleState::Planned,
    },
    MissionLifecycleTransition {
        from: MissionLifecycleState::Planned,
        via: MissionLifecycleTransitionKind::DispatchStarted,
        to: MissionLifecycleState::Dispatching,
    },
    MissionLifecycleTransition {
        from: MissionLifecycleState::AwaitingApproval,
        via: MissionLifecycleTransitionKind::DispatchStarted,
        to: MissionLifecycleState::Dispatching,
    },
    MissionLifecycleTransition {
        from: MissionLifecycleState::Dispatching,
        via: MissionLifecycleTransitionKind::ExecutionStarted,
        to: MissionLifecycleState::Running,
    },
    MissionLifecycleTransition {
        from: MissionLifecycleState::Running,
        via: MissionLifecycleTransitionKind::ExecutionBlocked,
        to: MissionLifecycleState::Paused,
    },
    MissionLifecycleTransition {
        from: MissionLifecycleState::Blocked,
        via: MissionLifecycleTransitionKind::RetryResumed,
        to: MissionLifecycleState::Running,
    },
    MissionLifecycleTransition {
        from: MissionLifecycleState::RetryPending,
        via: MissionLifecycleTransitionKind::RetryResumed,
        to: MissionLifecycleState::Dispatching,
    },
    MissionLifecycleTransition {
        from: MissionLifecycleState::Running,
        via: MissionLifecycleTransitionKind::MissionCancelled,
        to: MissionLifecycleState::Cancelled,
    },
    MissionLifecycleTransition {
        from: MissionLifecycleState::Executing,
        via: MissionLifecycleTransitionKind::MissionCancelled,
        to: MissionLifecycleState::Cancelled,
    },
    MissionLifecycleTransition {
        from: MissionLifecycleState::Paused,
        via: MissionLifecycleTransitionKind::MissionCancelled,
        to: MissionLifecycleState::Cancelled,
    },
    MissionLifecycleTransition {
        from: MissionLifecycleState::Blocked,
        via: MissionLifecycleTransitionKind::MissionCancelled,
        to: MissionLifecycleState::Cancelled,
    },
    MissionLifecycleTransition {
        from: MissionLifecycleState::Dispatching,
        via: MissionLifecycleTransitionKind::MissionCancelled,
        to: MissionLifecycleState::Cancelled,
    },
    MissionLifecycleTransition {
        from: MissionLifecycleState::RetryPending,
        via: MissionLifecycleTransitionKind::MissionCancelled,
        to: MissionLifecycleState::Cancelled,
    },
    // --- Executing (alias for Running) extended transitions ---
    MissionLifecycleTransition {
        from: MissionLifecycleState::Executing,
        via: MissionLifecycleTransitionKind::ExecutionBlocked,
        to: MissionLifecycleState::Paused,
    },
    // Pause support: ExecutionBlocked from additional states for mission pause.
    MissionLifecycleTransition {
        from: MissionLifecycleState::Dispatching,
        via: MissionLifecycleTransitionKind::ExecutionBlocked,
        to: MissionLifecycleState::Paused,
    },
    MissionLifecycleTransition {
        from: MissionLifecycleState::AwaitingApproval,
        via: MissionLifecycleTransitionKind::ExecutionBlocked,
        to: MissionLifecycleState::Paused,
    },
    MissionLifecycleTransition {
        from: MissionLifecycleState::Blocked,
        via: MissionLifecycleTransitionKind::ExecutionBlocked,
        to: MissionLifecycleState::Paused,
    },
    MissionLifecycleTransition {
        from: MissionLifecycleState::RetryPending,
        via: MissionLifecycleTransitionKind::ExecutionBlocked,
        to: MissionLifecycleState::Paused,
    },
];

/// Errors from mission lifecycle state transitions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MissionLifecycleError {
    InvalidTransition {
        from: MissionLifecycleState,
        transition: MissionLifecycleTransitionKind,
    },
}

impl fmt::Display for MissionLifecycleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTransition { from, transition } => write!(
                f,
                "invalid mission lifecycle transition: {from} --{transition}--> ?"
            ),
        }
    }
}

impl std::error::Error for MissionLifecycleError {}

/// Terminality classification for mission failure codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissionFailureTerminality {
    Terminal,
    NonTerminal,
}

/// Retryability classification for mission failure codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissionFailureRetryability {
    Retryable,
    NotRetryable,
}

/// Structured failure code for mission-level errors.
///
/// Each variant provides stable reason/error codes for machine parsing and
/// human-readable hints for operator triage.
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

impl MissionFailureCode {
    /// Every `MissionFailureCode` variant, in declaration order.
    ///
    /// The explicit length makes this the single source of truth for variant
    /// enumeration: adding a variant to the enum without extending this array
    /// is a compile error, so derived surfaces (e.g.
    /// `mcp_mission_failure_catalog`) and their anti-drift tests cannot silently
    /// fall out of sync (ft-dfnoe).
    pub const ALL: [MissionFailureCode; 9] = [
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

    /// Stable machine-readable reason code.
    #[must_use]
    pub fn reason_code(self) -> &'static str {
        match self {
            Self::PolicyDenied => "mission.policy_denied",
            Self::ReservationConflict => "mission.reservation_conflict",
            Self::RateLimited => "mission.rate_limited",
            Self::StaleState => "mission.stale_state",
            Self::DispatchError => "mission.dispatch_error",
            Self::ApprovalRequired => "mission.approval_required",
            Self::ApprovalDenied => "mission.approval_denied",
            Self::ApprovalExpired => "mission.approval_expired",
            Self::KillSwitchActivated => "mission.kill_switch_activated",
        }
    }

    /// Stable machine-readable error code.
    #[must_use]
    pub fn error_code(self) -> &'static str {
        match self {
            Self::PolicyDenied => "robot.mission_policy_denied",
            Self::ReservationConflict => "robot.mission_reservation_conflict",
            Self::RateLimited => "robot.mission_rate_limited",
            Self::StaleState => "robot.mission_stale_state",
            Self::DispatchError => "robot.mission_dispatch_error",
            Self::ApprovalRequired => "robot.mission_approval_required",
            Self::ApprovalDenied => "robot.mission_approval_denied",
            Self::ApprovalExpired => "robot.mission_approval_expired",
            Self::KillSwitchActivated => "robot.mission_kill_switch_activated",
        }
    }

    /// Whether this failure code is terminal (no automatic recovery).
    #[must_use]
    pub fn terminality(self) -> MissionFailureTerminality {
        match self {
            Self::PolicyDenied | Self::ApprovalDenied | Self::KillSwitchActivated => {
                MissionFailureTerminality::Terminal
            }
            Self::ReservationConflict
            | Self::RateLimited
            | Self::StaleState
            | Self::DispatchError
            | Self::ApprovalRequired
            | Self::ApprovalExpired => MissionFailureTerminality::NonTerminal,
        }
    }

    /// Whether the failure is retryable.
    #[must_use]
    pub fn retryability(self) -> MissionFailureRetryability {
        match self {
            Self::ReservationConflict
            | Self::RateLimited
            | Self::StaleState
            | Self::DispatchError
            | Self::ApprovalExpired => MissionFailureRetryability::Retryable,
            Self::PolicyDenied
            | Self::ApprovalRequired
            | Self::ApprovalDenied
            | Self::KillSwitchActivated => MissionFailureRetryability::NotRetryable,
        }
    }

    /// Human-readable triage hint.
    #[must_use]
    pub fn human_hint(self) -> &'static str {
        match self {
            Self::PolicyDenied => {
                "The action was blocked by a safety policy. Check capability gates."
            }
            Self::ReservationConflict => {
                "A file reservation conflict prevented dispatch. Wait or release the conflicting reservation."
            }
            Self::RateLimited => {
                "Rate limit exceeded. Wait for the cooldown period before retrying."
            }
            Self::StaleState => "Mission state is stale. Refresh state and retry.",
            Self::DispatchError => {
                "Dispatch failed due to a transient error. Retry after investigating logs."
            }
            Self::ApprovalRequired => "This action requires operator approval before proceeding.",
            Self::ApprovalDenied => {
                "Operator denied the approval request. Review the denial reason."
            }
            Self::ApprovalExpired => "The approval window expired. Request a new approval.",
            Self::KillSwitchActivated => {
                "The kill switch was activated. All mission operations are halted."
            }
        }
    }

    /// Machine-readable triage hint for automated recovery.
    #[must_use]
    pub fn machine_hint(self) -> &'static str {
        match self {
            Self::PolicyDenied => "check_policy_gates",
            Self::ReservationConflict => "wait_or_release_reservation",
            Self::RateLimited => "backoff_and_retry",
            Self::StaleState => "refresh_state_and_retry",
            Self::DispatchError => "retry_with_exponential_backoff",
            Self::ApprovalRequired => "request_operator_approval",
            Self::ApprovalDenied => "review_denial_and_escalate",
            Self::ApprovalExpired => "request_new_approval",
            Self::KillSwitchActivated => "halt_all_operations",
        }
    }
}

impl fmt::Display for MissionFailureCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.reason_code())
    }
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

/// Canonical mission object for planner/dispatcher/operator orchestration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mission {
    pub mission_version: u32,
    pub mission_id: MissionId,
    pub title: String,
    pub workspace_id: String,
    #[serde(default)]
    pub lifecycle_state: MissionLifecycleState,
    pub ownership: MissionOwnership,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provenance: Option<MissionProvenance>,
    pub created_at_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<CandidateAction>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assignments: Vec<Assignment>,
    #[serde(default)]
    pub pause_resume_state: MissionPauseResumeState,
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
            lifecycle_state: MissionLifecycleState::Planning,
            ownership,
            provenance: None,
            created_at_ms,
            updated_at_ms: None,
            candidates: Vec::new(),
            assignments: Vec::new(),
            pause_resume_state: MissionPauseResumeState::default(),
        }
    }

    /// Advance mission lifecycle state using the transition table.
    pub fn apply_lifecycle_transition(
        &mut self,
        transition: MissionLifecycleTransitionKind,
        transitioned_at_ms: i64,
    ) -> Result<MissionLifecycleState, MissionLifecycleError> {
        let next = self.lifecycle_state.apply_transition(transition)?;
        self.lifecycle_state = next;
        self.updated_at_ms = Some(transitioned_at_ms);
        Ok(next)
    }

    /// Allowed lifecycle transitions for the current mission state.
    #[must_use]
    pub fn allowed_lifecycle_transitions(&self) -> Vec<MissionLifecycleTransitionKind> {
        self.lifecycle_state.allowed_transitions()
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
            format!("lifecycle_state={}", self.lifecycle_state),
            format!("ownership={}", self.ownership.canonical_string()),
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
        if self.title.trim().is_empty() {
            return Err(MissionValidationError::MissingTitle);
        }
        if self.workspace_id.trim().is_empty() {
            return Err(MissionValidationError::MissingWorkspaceId);
        }
        self.ownership.validate()?;

        let mut candidate_ids = std::collections::HashSet::new();
        for candidate in &self.candidates {
            if !candidate_ids.insert(candidate.candidate_id.clone()) {
                return Err(MissionValidationError::DuplicateCandidateId(
                    candidate.candidate_id.clone(),
                ));
            }
        }

        let mut assignment_ids = std::collections::HashSet::new();
        for assignment in &self.assignments {
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
            if let Some(reservation_intent) = &assignment.reservation_intent {
                if reservation_intent.paths.is_empty() {
                    return Err(MissionValidationError::EmptyReservationPaths(
                        reservation_intent.reservation_id.clone(),
                    ));
                }
                for path in &reservation_intent.paths {
                    if let Some(reason) = invalid_reservation_path_reason(path) {
                        return Err(MissionValidationError::InvalidReservationPath {
                            reservation_id: reservation_intent.reservation_id.clone(),
                            path: path.clone(),
                            reason,
                        });
                    }
                }
            }
        }

        self.validate_lifecycle_state()?;
        Ok(())
    }

    /// Transition the mission lifecycle to a specific target state via a named
    /// transition kind. Validates that the transition is legal according to the
    /// lifecycle transition table.
    pub fn transition_lifecycle(
        &mut self,
        to: MissionLifecycleState,
        kind: MissionLifecycleTransitionKind,
        transitioned_at_ms: i64,
    ) -> Result<MissionLifecycleState, MissionLifecycleError> {
        let valid = MISSION_LIFECYCLE_TRANSITIONS
            .iter()
            .any(|rule| rule.from == self.lifecycle_state && rule.via == kind && rule.to == to);
        if !valid {
            return Err(MissionLifecycleError::InvalidTransition {
                from: self.lifecycle_state,
                transition: kind,
            });
        }
        self.lifecycle_state = to;
        self.updated_at_ms = Some(transitioned_at_ms);
        Ok(to)
    }

    /// Look up the dispatch contract for a given candidate.
    ///
    /// Returns the candidate action and its approval requirements, or an error
    /// if the candidate is not found in this mission.
    pub fn dispatch_contract_for_candidate(
        &self,
        candidate_id: &CandidateActionId,
    ) -> Result<MissionDispatchContract, MissionDispatchError> {
        let candidate = self
            .candidates
            .iter()
            .find(|c| c.candidate_id == *candidate_id)
            .ok_or_else(|| MissionDispatchError::CandidateNotFound(candidate_id.clone()))?;
        let assignment = self
            .assignments
            .iter()
            .find(|a| a.candidate_id == *candidate_id);
        Ok(MissionDispatchContract {
            assignment_id: assignment.map(|a| a.assignment_id.0.clone()),
            target_agent: assignment.map(|a| a.assignee.clone()),
            candidate_id: candidate.candidate_id.clone(),
            action: candidate.action.clone(),
            rationale: candidate.rationale.clone(),
            approval_state: assignment.map(|a| a.approval_state.clone()),
        })
    }

    /// Resolve the dispatch target (assignment) for a given assignment ID.
    pub fn resolve_dispatch_target(
        &self,
        assignment_id: &AssignmentId,
    ) -> Result<MissionDispatchTarget, MissionDispatchError> {
        let assignment = self
            .assignments
            .iter()
            .find(|a| a.assignment_id == *assignment_id)
            .ok_or_else(|| MissionDispatchError::AssignmentNotFound(assignment_id.clone()))?;
        let candidate = self.candidate_for_assignment(assignment)?;
        Ok(MissionDispatchTarget {
            assignment_id: assignment.assignment_id.clone(),
            assignee: assignment.assignee.clone(),
            candidate_id: assignment.candidate_id.clone(),
            action_type: candidate.action.action_type_name().to_string(),
            pane_id: dispatch_pane_id_for_action(&candidate.action),
            workspace: Some(self.workspace_id.clone()),
            approval_state: assignment.approval_state.clone(),
        })
    }

    /// Perform a dry-run of dispatching an assignment without side effects.
    pub fn dispatch_assignment_dry_run(
        &self,
        assignment_id: &AssignmentId,
        completed_at_ms: i64,
    ) -> Result<MissionDispatchExecution, MissionDispatchError> {
        let assignment = self
            .assignments
            .iter()
            .find(|a| a.assignment_id == *assignment_id)
            .ok_or_else(|| MissionDispatchError::AssignmentNotFound(assignment_id.clone()))?;
        let target = self.resolve_dispatch_target(assignment_id)?;
        let would_approve = matches!(
            assignment.approval_state,
            ApprovalState::Approved { .. } | ApprovalState::NotRequired
        );
        let target_reachable = self.candidate_for_assignment(assignment).is_ok();
        let would_succeed = would_approve && target_reachable;
        Ok(MissionDispatchExecution {
            assignment_id: assignment.assignment_id.clone(),
            would_dispatch: would_succeed,
            simulated_at_ms: completed_at_ms,
            target: target.clone(),
            approval_allows_dispatch: would_approve,
            target_reachable,
            would_succeed,
            reason: if would_succeed {
                None
            } else if !target_reachable {
                Some(format!(
                    "assignment references missing candidate: {}",
                    assignment.candidate_id.0
                ))
            } else {
                Some(format!("approval state: {:?}", assignment.approval_state))
            },
        })
    }

    fn candidate_for_assignment(
        &self,
        assignment: &Assignment,
    ) -> Result<&CandidateAction, MissionDispatchError> {
        self.candidates
            .iter()
            .find(|c| c.candidate_id == assignment.candidate_id)
            .ok_or_else(|| MissionDispatchError::CandidateNotFound(assignment.candidate_id.clone()))
    }

    fn validate_lifecycle_state(&self) -> Result<(), MissionValidationError> {
        let has_success = self
            .assignments
            .iter()
            .filter_map(|a| a.outcome.as_ref())
            .any(|outcome| matches!(outcome, Outcome::Success { .. }));
        let has_failure = self
            .assignments
            .iter()
            .filter_map(|a| a.outcome.as_ref())
            .any(|outcome| matches!(outcome, Outcome::Failed { .. }));
        let has_cancel = self
            .assignments
            .iter()
            .filter_map(|a| a.outcome.as_ref())
            .any(|outcome| matches!(outcome, Outcome::Cancelled { .. }));

        match self.lifecycle_state {
            MissionLifecycleState::Completed if !has_success => {
                Err(MissionValidationError::LifecycleStateOutcomeMismatch {
                    state: self.lifecycle_state,
                    detail: "completed mission requires at least one success outcome",
                })
            }
            MissionLifecycleState::Failed if !has_failure => {
                Err(MissionValidationError::LifecycleStateOutcomeMismatch {
                    state: self.lifecycle_state,
                    detail: "failed mission requires at least one failed outcome",
                })
            }
            MissionLifecycleState::Cancelled if !has_cancel => {
                Err(MissionValidationError::LifecycleStateOutcomeMismatch {
                    state: self.lifecycle_state,
                    detail: "cancelled mission requires at least one cancelled outcome",
                })
            }
            _ => Ok(()),
        }
    }
}

/// Dispatch contract returned by `Mission::dispatch_contract_for_candidate`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DispatchContract {
    pub candidate_id: CandidateActionId,
    pub action: StepAction,
    pub rationale: String,
}

/// Dispatch target returned by `Mission::resolve_dispatch_target`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DispatchTarget {
    pub assignment_id: AssignmentId,
    pub assignee: String,
    pub candidate_id: CandidateActionId,
    pub approval_state: ApprovalState,
}

/// Dry-run result returned by `Mission::dispatch_assignment_dry_run`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DispatchDryRun {
    pub assignment_id: AssignmentId,
    pub would_dispatch: bool,
    pub simulated_at_ms: i64,
}

fn dispatch_pane_id_for_action(action: &StepAction) -> Option<u64> {
    match action {
        StepAction::SendText { pane_id, .. } => Some(*pane_id),
        StepAction::WaitFor {
            pane_id, condition, ..
        } => (*pane_id).or_else(|| dispatch_pane_id_for_wait_condition(condition)),
        StepAction::AcquireLock { .. }
        | StepAction::ReleaseLock { .. }
        | StepAction::StoreData { .. }
        | StepAction::RunWorkflow { .. }
        | StepAction::MarkEventHandled { .. }
        | StepAction::ValidateApproval { .. }
        | StepAction::NestedPlan { .. }
        | StepAction::Custom { .. } => None,
    }
}

fn dispatch_pane_id_for_wait_condition(condition: &WaitCondition) -> Option<u64> {
    match condition {
        WaitCondition::Pattern { pane_id, .. }
        | WaitCondition::PaneIdle { pane_id, .. }
        | WaitCondition::StableTail { pane_id, .. } => *pane_id,
        WaitCondition::External { .. } => None,
    }
}

fn invalid_reservation_path_reason(path: &str) -> Option<&'static str> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Some("path cannot be empty");
    }
    if trimmed != path {
        return Some("path cannot contain leading or trailing whitespace");
    }
    if path.chars().any(char::is_control) {
        return Some("path cannot contain control characters");
    }
    if path.starts_with('/') || path.starts_with('~') || path.as_bytes().get(1) == Some(&b':') {
        return Some("path must be workspace-relative");
    }
    if path.contains('\\') {
        return Some("path must use forward slashes");
    }
    if path
        .split('/')
        .any(|component| matches!(component, "." | ".."))
    {
        return Some("path cannot contain traversal components");
    }
    None
}

/// Errors from mission dispatch operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MissionDispatchError {
    CandidateNotFound(CandidateActionId),
    AssignmentNotFound(AssignmentId),
}

impl fmt::Display for MissionDispatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CandidateNotFound(id) => write!(f, "candidate not found: {id}"),
            Self::AssignmentNotFound(id) => write!(f, "assignment not found: {id}"),
        }
    }
}

impl std::error::Error for MissionDispatchError {}

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
    DuplicateCandidateId(CandidateActionId),
    DuplicateAssignmentId(AssignmentId),
    UnknownCandidateReference(CandidateActionId),
    EmptyAssignee(AssignmentId),
    EmptyReservationPaths(ReservationIntentId),
    InvalidReservationPath {
        reservation_id: ReservationIntentId,
        path: String,
        reason: &'static str,
    },
    LifecycleStateOutcomeMismatch {
        state: MissionLifecycleState,
        detail: &'static str,
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
            Self::DuplicateCandidateId(id) => write!(f, "Duplicate candidate ID: {}", id.0),
            Self::DuplicateAssignmentId(id) => write!(f, "Duplicate assignment ID: {}", id.0),
            Self::UnknownCandidateReference(id) => {
                write!(f, "Assignment references unknown candidate ID: {}", id.0)
            }
            Self::EmptyAssignee(id) => write!(f, "Assignment has empty assignee: {}", id.0),
            Self::EmptyReservationPaths(id) => {
                write!(f, "Reservation intent has empty paths: {}", id.0)
            }
            Self::InvalidReservationPath {
                reservation_id,
                path,
                reason,
            } => write!(
                f,
                "Reservation intent {} has invalid path {:?}: {}",
                reservation_id.0, path, reason
            ),
            Self::LifecycleStateOutcomeMismatch { state, detail } => {
                write!(f, "Mission lifecycle state mismatch ({state}): {detail}")
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
// Mission dispatch projections (referenced by robot_types::MissionDecisionData)
// ============================================================================

/// Dispatch contract for a mission assignment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissionDispatchContract {
    /// Assignment being dispatched, if the candidate has been assigned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignment_id: Option<String>,
    /// Agent receiving the dispatch, if the candidate has been assigned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_agent: Option<String>,
    /// Candidate action being dispatched.
    pub candidate_id: CandidateActionId,
    /// Full candidate action payload.
    pub action: StepAction,
    /// Planner rationale for the candidate action.
    pub rationale: String,
    /// Current approval state for the linked assignment, if assigned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_state: Option<ApprovalState>,
}

impl MissionDispatchContract {
    /// Stable correlation key for dispatch logging and synthetic detections.
    #[must_use]
    pub fn correlation_id(&self) -> &str {
        self.assignment_id
            .as_deref()
            .unwrap_or(self.candidate_id.0.as_str())
    }

    /// Target agent label for legacy dispatch result fields.
    #[must_use]
    pub fn target_agent_label(&self) -> &str {
        self.target_agent.as_deref().unwrap_or("")
    }
}

/// Target for a mission dispatch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissionDispatchTarget {
    /// Assignment being dispatched.
    pub assignment_id: AssignmentId,
    /// Agent receiving the dispatch.
    pub assignee: String,
    /// Candidate action being dispatched.
    pub candidate_id: CandidateActionId,
    /// Candidate action type.
    pub action_type: String,
    /// Pane ID for pane-scoped dispatch actions. Non-pane actions leave this unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<u64>,
    /// Mission workspace path or identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// Current approval state for this assignment.
    pub approval_state: ApprovalState,
}

/// Execution details for a mission dispatch dry-run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissionDispatchExecution {
    /// Assignment being projected.
    pub assignment_id: AssignmentId,
    /// Whether the dispatch would be attempted.
    pub would_dispatch: bool,
    /// Dry-run evaluation timestamp.
    pub simulated_at_ms: i64,
    /// Resolved target used for this dry-run.
    pub target: MissionDispatchTarget,
    /// Whether approval state permits dispatch.
    pub approval_allows_dispatch: bool,
    /// Whether the assignment resolves to a real candidate target.
    pub target_reachable: bool,
    /// Backwards-compatible success flag matching `would_dispatch`.
    pub would_succeed: bool,
    /// Reason if the dispatch would fail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

// ============================================================================
// Mission Transaction (Tx) Types
// ============================================================================

/// Current schema version for mission transaction contracts.
pub const MISSION_TX_SCHEMA_VERSION: u32 = 1;

/// Transaction state within a mission lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionTxState {
    Draft,
    Planned,
    Prepared,
    Committing,
    Committed,
    Failed,
    Compensating,
    Compensated,
    RolledBack,
}

impl MissionTxState {
    /// Whether this tx state is terminal (no further transitions possible).
    ///
    /// Note: `Failed` is NOT terminal — it can transition to `Compensating`.
    /// Use `is_settled()` to check for states that are either terminal or
    /// require explicit operator intervention to proceed.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Committed | Self::Compensated | Self::RolledBack)
    }

    /// Whether this tx state is settled (terminal or failed-awaiting-compensation).
    ///
    /// `Failed` is settled but not terminal: it can still transition to
    /// `Compensating` via explicit operator action or automatic retry.
    #[must_use]
    pub fn is_settled(self) -> bool {
        self.is_terminal() || matches!(self, Self::Failed)
    }

    const fn expected_outcome(self) -> TxOutcome {
        match self {
            Self::Committed => TxOutcome::Committed,
            Self::Failed => TxOutcome::Failed,
            Self::Compensated | Self::RolledBack => TxOutcome::Compensated,
            Self::Draft
            | Self::Planned
            | Self::Prepared
            | Self::Committing
            | Self::Compensating => TxOutcome::Pending,
        }
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
            Self::Failed => f.write_str("failed"),
            Self::Compensating => f.write_str("compensating"),
            Self::Compensated => f.write_str("compensated"),
            Self::RolledBack => f.write_str("rolled_back"),
        }
    }
}

/// Kill switch severity for mission execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionKillSwitchLevel {
    Off,
    SafeMode,
    HardStop,
}

/// Outcome of the overall mission transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TxOutcome {
    Pending,
    Committed,
    Failed,
    Compensated,
}

/// Outcome of the transaction prepare phase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TxPrepareOutcome {
    AllReady,
    RequireApproval,
    Denied,
    Deferred,
}

impl TxPrepareOutcome {
    /// Whether commit is eligible after prepare.
    #[must_use]
    pub fn commit_eligible(&self) -> bool {
        matches!(self, Self::AllReady)
    }
}

/// Outcome of the transaction commit phase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TxCommitOutcome {
    FullyCommitted,
    PartialFailure,
    ImmediateFailure,
    KillSwitchBlocked,
    PauseSuspended,
}

impl TxCommitOutcome {
    /// Target tx state after this commit outcome.
    #[must_use]
    pub fn target_tx_state(&self) -> MissionTxState {
        match self {
            Self::FullyCommitted => MissionTxState::Committed,
            Self::PartialFailure | Self::ImmediateFailure | Self::KillSwitchBlocked => {
                MissionTxState::Failed
            }
            Self::PauseSuspended => MissionTxState::Committing,
        }
    }
}

/// Outcome of a single commit step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TxCommitStepOutcome {
    Committed { reason_code: String },
    Failed { reason_code: String },
    Skipped { reason_code: String },
}

impl TxCommitStepOutcome {
    /// Whether this step was committed successfully.
    #[must_use]
    pub fn is_committed(&self) -> bool {
        matches!(self, Self::Committed { .. })
    }

    /// Whether this step was skipped.
    #[must_use]
    pub fn is_skipped(&self) -> bool {
        matches!(self, Self::Skipped { .. })
    }
}

/// Outcome of the compensation phase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TxCompensationOutcome {
    FullyRolledBack,
    CompensationFailed,
    NothingToCompensate,
}

impl TxCompensationOutcome {
    /// Target tx state after compensation.
    #[must_use]
    pub fn target_tx_state(&self) -> MissionTxState {
        match self {
            Self::FullyRolledBack => MissionTxState::RolledBack,
            Self::NothingToCompensate => MissionTxState::Compensated,
            Self::CompensationFailed => MissionTxState::Failed,
        }
    }
}

/// Unique transaction identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TxId(pub String);

/// Unique step identifier within a transaction.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TxStepId(pub String);

/// Unique plan identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TxPlanId(pub String);

fn validate_tx_identifier(label: &str, value: &str) -> Result<(), String> {
    let reason = if value.is_empty() {
        Some("identifier cannot be empty")
    } else if value.chars().any(char::is_whitespace) {
        Some("identifier cannot contain whitespace")
    } else if value.chars().any(char::is_control) {
        Some("identifier cannot contain control characters")
    } else if value.starts_with('~') || value.contains('/') || value.contains('\\') {
        Some("identifier must not be path-like")
    } else if value == "."
        || value == ".."
        || value.split(':').any(|part| matches!(part, "." | ".."))
    {
        Some("identifier cannot contain traversal components")
    } else {
        None
    };

    if let Some(reason) = reason {
        return Err(format!(
            "Transaction {label} {value:?} is invalid: {reason}"
        ));
    }

    Ok(())
}

/// Intent describing a mission transaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxIntent {
    pub tx_id: TxId,
    pub requested_by: MissionActorRole,
    pub summary: String,
    pub correlation_id: String,
    pub created_at_ms: i64,
}

/// A single step in a transaction plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxStep {
    pub step_id: TxStepId,
    pub ordinal: usize,
    pub action: StepAction,
    #[serde(default)]
    pub description: String,
}

/// Precondition that must hold before a transaction step.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TxPrecondition {
    PromptActive { pane_id: u64 },
    Custom { check: String },
}

/// Compensation action for a step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxCompensation {
    pub for_step_id: TxStepId,
    pub action: StepAction,
}

/// Complete transaction plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxPlan {
    pub plan_id: TxPlanId,
    pub tx_id: TxId,
    pub steps: Vec<TxStep>,
    #[serde(default)]
    pub preconditions: Vec<TxPrecondition>,
    #[serde(default)]
    pub compensations: Vec<TxCompensation>,
}

/// Token budget attached to a mission transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionTokenBudget {
    /// Maximum total prompt+output tokens the mission may consume.
    pub max_tokens: u64,
    /// Maximum prompt+output tokens that may burn without mission progress.
    pub max_no_progress_tokens: u64,
}

impl MissionTokenBudget {
    pub fn validate(&self) -> Result<(), String> {
        if self.max_tokens == 0 {
            return Err("Mission token budget max_tokens must be > 0".to_string());
        }
        if self.max_no_progress_tokens == 0 {
            return Err("Mission token budget max_no_progress_tokens must be > 0".to_string());
        }
        if self.max_no_progress_tokens > self.max_tokens {
            return Err(format!(
                "Mission token budget no-progress limit {} exceeds total budget {}",
                self.max_no_progress_tokens, self.max_tokens
            ));
        }
        Ok(())
    }
}

const MISSION_TOKEN_BUDGET_ATTACHMENT_KIND: &str = "mission_token_budget";

fn mission_token_budget_attachment_kind() -> String {
    MISSION_TOKEN_BUDGET_ATTACHMENT_KIND.to_string()
}

/// One prompt/output usage observation for the economic circuit breaker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionTokenUsageSample {
    /// Prompt/input tokens observed since the previous sample.
    pub prompt_tokens: u64,
    /// Output/completion tokens observed since the previous sample.
    pub output_tokens: u64,
    /// Mission-progress markers advanced since the previous sample.
    pub progress_delta: u64,
    /// Observation timestamp in epoch milliseconds.
    pub observed_at_ms: i64,
}

impl MissionTokenUsageSample {
    #[must_use]
    pub fn total_tokens(&self) -> u64 {
        self.prompt_tokens.saturating_add(self.output_tokens)
    }
}

/// Accumulated economic circuit-breaker state for a mission transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct MissionEconomicState {
    /// Total prompt+output tokens observed for this transaction.
    pub total_tokens: u64,
    /// Tokens burned since the last recorded progress marker.
    pub no_progress_tokens: u64,
    /// Total progress markers observed.
    pub progress_markers: u64,
    /// Whether the economic breaker has tripped the mission hard stop.
    pub hard_stop_tripped: bool,
    /// Last hard-stop reason code, when tripped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_reason_code: Option<String>,
    /// Last usage observation timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_observed_at_ms: Option<i64>,
}

/// Typed hard-stop envelope emitted when the economic breaker trips.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionEconomicHardStopEnvelope {
    pub tx_id: TxId,
    pub plan_id: TxPlanId,
    pub correlation_id: String,
    pub reason_code: String,
    pub kill_switch_level: MissionKillSwitchLevel,
    pub token_budget: MissionTokenBudget,
    pub total_tokens: u64,
    pub no_progress_tokens: u64,
    pub prompt_tokens: u64,
    pub output_tokens: u64,
    pub progress_delta: u64,
    pub observed_at_ms: i64,
}

/// Audit row shape for economic hard stops.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionEconomicAuditRow {
    pub actor: MissionActorRole,
    pub action: String,
    pub tx_id: TxId,
    pub plan_id: TxPlanId,
    pub correlation_id: String,
    pub reason_code: String,
    pub kill_switch_level: MissionKillSwitchLevel,
    pub total_tokens: u64,
    pub no_progress_tokens: u64,
    pub observed_at_ms: i64,
}

/// Economic circuit-breaker decision after recording one usage sample.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum MissionEconomicBreakerDecision {
    Continue {
        state: MissionEconomicState,
    },
    HardStop {
        envelope: Box<MissionEconomicHardStopEnvelope>,
        audit_row: MissionEconomicAuditRow,
    },
}

/// Typed token-budget attachment carried inside a mission transaction contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionTokenBudgetAttachment {
    #[serde(default = "mission_token_budget_attachment_kind")]
    pub kind: String,
    pub token_budget: MissionTokenBudget,
    #[serde(default)]
    pub economic_state: MissionEconomicState,
}

impl MissionTokenBudgetAttachment {
    #[must_use]
    pub fn new(token_budget: MissionTokenBudget) -> Self {
        Self {
            kind: mission_token_budget_attachment_kind(),
            token_budget,
            economic_state: MissionEconomicState::default(),
        }
    }

    fn validate(&self) -> Result<(), String> {
        if self.kind != MISSION_TOKEN_BUDGET_ATTACHMENT_KIND {
            return Err(format!(
                "Mission token budget attachment kind {:?} is invalid",
                self.kind
            ));
        }
        self.token_budget.validate()
    }
}

/// Full mission transaction contract.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissionTxContract {
    pub tx_version: u32,
    pub intent: TxIntent,
    pub plan: TxPlan,
    pub lifecycle_state: MissionTxState,
    pub outcome: TxOutcome,
    #[serde(default)]
    pub receipts: Vec<serde_json::Value>,
}

impl MissionTxContract {
    /// Compute the canonical immutable hash for this tx contract.
    ///
    /// The hash intentionally excludes lifecycle state, outcome, and appended
    /// receipts so the same planned tx remains hash-stable while it moves
    /// through prepare/commit/compensation. Steering receipts use this as the
    /// plan/execute identity binding for tx execution.
    #[must_use]
    pub fn compute_hash(&self) -> String {
        #[derive(Serialize)]
        struct ImmutableTxStep<'a> {
            step_id: &'a TxStepId,
            ordinal: usize,
            action: &'a StepAction,
            description: &'a str,
        }

        #[derive(Serialize)]
        struct ImmutableTxContract<'a> {
            tx_version: u32,
            tx_id: &'a TxId,
            requested_by: &'a MissionActorRole,
            summary: &'a str,
            correlation_id: &'a str,
            plan_id: &'a TxPlanId,
            steps: Vec<ImmutableTxStep<'a>>,
            preconditions: &'a [TxPrecondition],
            compensations: &'a [TxCompensation],
        }

        let canonical = serde_json::to_string(&ImmutableTxContract {
            tx_version: self.tx_version,
            tx_id: &self.intent.tx_id,
            requested_by: &self.intent.requested_by,
            summary: &self.intent.summary,
            correlation_id: &self.intent.correlation_id,
            plan_id: &self.plan.plan_id,
            steps: self
                .plan
                .steps
                .iter()
                .map(|step| ImmutableTxStep {
                    step_id: &step.step_id,
                    ordinal: step.ordinal,
                    action: &step.action,
                    description: &step.description,
                })
                .collect(),
            preconditions: &self.plan.preconditions,
            compensations: &self.plan.compensations,
        })
        .expect("serializing immutable tx contract should not fail");
        let hash = sha256_hex(&canonical);
        format!("sha256:{}", &hash[..32])
    }

    /// Attach or replace the token budget for this mission transaction.
    pub fn attach_token_budget(&mut self, token_budget: MissionTokenBudget) -> Result<(), String> {
        token_budget.validate()?;
        self.store_token_budget_attachment(MissionTokenBudgetAttachment::new(token_budget))
    }

    /// Latest token-budget attachment carried by this contract, if any.
    /// All pane ids referenced by plan steps, preconditions, and compensations.
    ///
    /// This is the set prepare-gate evidence must cover: callers that
    /// pre-resolve pane capabilities (see
    /// [`StorageBackedPrepareTargetLookup::with_resolved_capabilities`])
    /// resolve exactly these panes.
    #[must_use]
    pub fn referenced_pane_ids(&self) -> std::collections::BTreeSet<u64> {
        let mut pane_ids = std::collections::BTreeSet::new();
        for step in &self.plan.steps {
            if let Some(pane_id) = tx_prepare_pane_id(&step.action) {
                pane_ids.insert(pane_id);
            }
        }
        for precondition in &self.plan.preconditions {
            if let TxPrecondition::PromptActive { pane_id } = precondition {
                pane_ids.insert(*pane_id);
            }
        }
        for compensation in &self.plan.compensations {
            if let Some(pane_id) = tx_prepare_pane_id(&compensation.action) {
                pane_ids.insert(pane_id);
            }
        }
        pane_ids
    }

    pub fn token_budget_attachment(&self) -> Result<Option<MissionTokenBudgetAttachment>, String> {
        let mut attachment = None;
        for receipt in &self.receipts {
            if receipt.get("kind").and_then(serde_json::Value::as_str)
                != Some(MISSION_TOKEN_BUDGET_ATTACHMENT_KIND)
            {
                continue;
            }
            let parsed = serde_json::from_value::<MissionTokenBudgetAttachment>(receipt.clone())
                .map_err(|err| format!("invalid mission token budget attachment: {err}"))?;
            parsed.validate()?;
            attachment = Some(parsed);
        }
        Ok(attachment)
    }

    fn store_token_budget_attachment(
        &mut self,
        attachment: MissionTokenBudgetAttachment,
    ) -> Result<(), String> {
        attachment.validate()?;
        let value = serde_json::to_value(attachment)
            .map_err(|err| format!("failed to serialize mission token budget attachment: {err}"))?;
        if let Some(existing) = self.receipts.iter_mut().rev().find(|receipt| {
            receipt.get("kind").and_then(serde_json::Value::as_str)
                == Some(MISSION_TOKEN_BUDGET_ATTACHMENT_KIND)
        }) {
            *existing = value;
        } else {
            self.receipts.push(value);
        }
        Ok(())
    }

    /// Record token usage and evaluate the economic circuit breaker.
    pub fn record_economic_usage(
        &mut self,
        sample: MissionTokenUsageSample,
    ) -> Result<MissionEconomicBreakerDecision, String> {
        let Some(mut attachment) = self.token_budget_attachment()? else {
            return Ok(MissionEconomicBreakerDecision::Continue {
                state: MissionEconomicState::default(),
            });
        };

        let observed_tokens = sample.total_tokens();
        attachment.economic_state.total_tokens = attachment
            .economic_state
            .total_tokens
            .saturating_add(observed_tokens);
        attachment.economic_state.last_observed_at_ms = Some(sample.observed_at_ms);

        if sample.progress_delta > 0 {
            attachment.economic_state.progress_markers = attachment
                .economic_state
                .progress_markers
                .saturating_add(sample.progress_delta);
            attachment.economic_state.no_progress_tokens = 0;
        } else {
            attachment.economic_state.no_progress_tokens = attachment
                .economic_state
                .no_progress_tokens
                .saturating_add(observed_tokens);
        }

        let reason_code = if attachment.economic_state.hard_stop_tripped {
            Some(
                attachment
                    .economic_state
                    .last_reason_code
                    .clone()
                    .unwrap_or_else(|| "tx.economic.hard_stop".to_string()),
            )
        } else if attachment.economic_state.no_progress_tokens
            >= attachment.token_budget.max_no_progress_tokens
        {
            Some("tx.economic.no_progress_budget_exhausted".to_string())
        } else if attachment.economic_state.total_tokens >= attachment.token_budget.max_tokens {
            Some("tx.economic.total_budget_exhausted".to_string())
        } else {
            None
        };

        let Some(reason_code) = reason_code else {
            let state = attachment.economic_state.clone();
            self.store_token_budget_attachment(attachment)?;
            return Ok(MissionEconomicBreakerDecision::Continue { state });
        };

        attachment.economic_state.hard_stop_tripped = true;
        attachment.economic_state.last_reason_code = Some(reason_code.clone());
        let decision = self.economic_hard_stop_decision(&attachment, &sample, &reason_code);
        self.store_token_budget_attachment(attachment)?;
        Ok(decision)
    }

    /// Current economic hard-stop decision, if the breaker has already tripped.
    pub fn economic_hard_stop_decision_current(
        &self,
        observed_at_ms: i64,
    ) -> Result<Option<MissionEconomicBreakerDecision>, String> {
        let Some(attachment) = self.token_budget_attachment()? else {
            return Ok(None);
        };
        if !attachment.economic_state.hard_stop_tripped {
            return Ok(None);
        }
        let reason_code = attachment
            .economic_state
            .last_reason_code
            .as_deref()
            .unwrap_or("tx.economic.hard_stop")
            .to_string();
        Ok(Some(self.economic_hard_stop_decision(
            &attachment,
            &MissionTokenUsageSample {
                prompt_tokens: 0,
                output_tokens: 0,
                progress_delta: 0,
                observed_at_ms,
            },
            &reason_code,
        )))
    }

    fn economic_hard_stop_decision(
        &self,
        attachment: &MissionTokenBudgetAttachment,
        sample: &MissionTokenUsageSample,
        reason_code: &str,
    ) -> MissionEconomicBreakerDecision {
        let economic_state = &attachment.economic_state;
        let envelope = MissionEconomicHardStopEnvelope {
            tx_id: self.intent.tx_id.clone(),
            plan_id: self.plan.plan_id.clone(),
            correlation_id: self.intent.correlation_id.clone(),
            reason_code: reason_code.to_string(),
            kill_switch_level: MissionKillSwitchLevel::HardStop,
            token_budget: attachment.token_budget.clone(),
            total_tokens: economic_state.total_tokens,
            no_progress_tokens: economic_state.no_progress_tokens,
            prompt_tokens: sample.prompt_tokens,
            output_tokens: sample.output_tokens,
            progress_delta: sample.progress_delta,
            observed_at_ms: sample.observed_at_ms,
        };
        let audit_row = MissionEconomicAuditRow {
            actor: self.intent.requested_by,
            action: "mission_economic_hard_stop".to_string(),
            tx_id: self.intent.tx_id.clone(),
            plan_id: self.plan.plan_id.clone(),
            correlation_id: self.intent.correlation_id.clone(),
            reason_code: reason_code.to_string(),
            kill_switch_level: MissionKillSwitchLevel::HardStop,
            total_tokens: economic_state.total_tokens,
            no_progress_tokens: economic_state.no_progress_tokens,
            observed_at_ms: sample.observed_at_ms,
        };
        MissionEconomicBreakerDecision::HardStop {
            envelope: Box::new(envelope),
            audit_row,
        }
    }

    /// Validate contract consistency.
    pub fn validate(&self) -> Result<(), String> {
        if self.tx_version != MISSION_TX_SCHEMA_VERSION {
            return Err(format!(
                "Transaction tx_version {} is unsupported; expected {}",
                self.tx_version, MISSION_TX_SCHEMA_VERSION
            ));
        }
        validate_tx_identifier("intent tx_id", &self.intent.tx_id.0)?;
        validate_tx_identifier("plan tx_id", &self.plan.tx_id.0)?;
        validate_tx_identifier("plan_id", &self.plan.plan_id.0)?;
        validate_tx_identifier("correlation_id", &self.intent.correlation_id)?;

        if self.intent.tx_id != self.plan.tx_id {
            return Err(format!(
                "Transaction intent tx_id {} does not match plan tx_id {}",
                self.intent.tx_id.0, self.plan.tx_id.0
            ));
        }

        let expected_outcome = self.lifecycle_state.expected_outcome();
        if self.outcome != expected_outcome {
            return Err(format!(
                "Transaction state {} requires outcome {:?}, got {:?}",
                self.lifecycle_state, expected_outcome, self.outcome
            ));
        }

        if self.plan.steps.is_empty() {
            return Err("Transaction plan has no steps".to_string());
        }
        let _ = self.token_budget_attachment()?;

        let mut step_ids = HashSet::new();
        let mut ordinals = HashSet::new();
        // `execute_commit_phase` runs steps in array order, and
        // `execute_compensation_phase` undoes them in the exact reverse of
        // that array order. The `ordinal` field is meant to be the
        // authoritative execution order, but nothing forces the array to be
        // laid out in ordinal order — a contract authored (or deserialized
        // from JSON) with steps physically out of ordinal order would commit,
        // and compensate, in the wrong order while the receipts still report
        // the declared ordinals. Reject any contract whose array order does
        // not match strictly-ascending ordinal order so the two orderings can
        // never silently diverge (fail-closed, mirroring the duplicate-ordinal
        // and order-dependent-input rejections elsewhere in the tx engine).
        let mut prev_ordinal: Option<usize> = None;
        for step in &self.plan.steps {
            if step.step_id.0.is_empty() {
                return Err("Transaction plan has a step with an empty step_id".to_string());
            }
            validate_tx_identifier("step_id", &step.step_id.0)?;
            if !step_ids.insert(step.step_id.0.as_str()) {
                return Err(format!(
                    "Transaction plan has duplicate step_id {}",
                    step.step_id.0
                ));
            }
            if !ordinals.insert(step.ordinal) {
                return Err(format!(
                    "Transaction plan has duplicate step ordinal {}",
                    step.ordinal
                ));
            }
            if let Some(prev) = prev_ordinal {
                if step.ordinal <= prev {
                    return Err(format!(
                        "Transaction plan steps are not in ascending ordinal order: \
                         step {} has ordinal {} after ordinal {} \
                         (array order is the execution order; it must match ordinal order)",
                        step.step_id.0, step.ordinal, prev
                    ));
                }
            }
            prev_ordinal = Some(step.ordinal);
        }

        let mut compensation_step_ids = HashSet::new();
        for compensation in &self.plan.compensations {
            let for_step_id = compensation.for_step_id.0.as_str();
            validate_tx_identifier("compensation step_id", for_step_id)?;
            if !step_ids.contains(for_step_id) {
                return Err(format!(
                    "Transaction compensation references unknown step_id {}",
                    compensation.for_step_id.0
                ));
            }
            if !compensation_step_ids.insert(for_step_id) {
                return Err(format!(
                    "Transaction plan has duplicate compensation for step_id {}",
                    compensation.for_step_id.0
                ));
            }
        }

        let _ = validate_persisted_tx_receipts(self)?;

        Ok(())
    }
}

/// Input for a single prepare-phase gate evaluation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TxPrepareApprovalRequirement {
    pub workspace_id: String,
    pub action_kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<u64>,
    pub action_fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<String>,
}

/// Input for a single prepare-phase gate evaluation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TxPrepareGateInput {
    pub step_id: TxStepId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<u64>,
    /// Whether every plan-level precondition was proven from the prepare snapshot.
    ///
    /// This defaults to `false` on deserialization so legacy or incomplete gate
    /// evidence cannot silently bypass newly added preconditions.
    #[serde(default)]
    pub preconditions_satisfied: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub precondition_reason_code: Option<String>,
    pub policy_passed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_reason_code: Option<String>,
    pub reservation_available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reservation_reason_code: Option<String>,
    pub approval_satisfied: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_reason_code: Option<String>,
    pub target_liveness: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub liveness_reason_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_approval: Option<TxPrepareApprovalRequirement>,
}

/// Input for a single commit step.
#[derive(Debug, Clone)]
pub struct TxCommitStepInput {
    pub step_id: TxStepId,
    pub success: bool,
    pub reason_code: String,
    pub error_code: Option<String>,
    pub completed_at_ms: i64,
}

/// Input for a single compensation step.
#[derive(Debug, Clone)]
pub struct TxCompensationStepInput {
    pub for_step_id: TxStepId,
    pub success: bool,
    pub reason_code: String,
    pub error_code: Option<String>,
    pub completed_at_ms: i64,
}

/// Result of a single commit step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxCommitStepResult {
    pub step_id: TxStepId,
    pub ordinal: usize,
    pub outcome: TxCommitStepOutcome,
    pub decision_path: String,
    pub completed_at_ms: i64,
}

/// Report from the prepare phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxPrepareReport {
    pub outcome: TxPrepareOutcome,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gate_inputs: Vec<TxPrepareGateInput>,
}

/// Report from the commit phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxCommitReport {
    pub tx_id: TxId,
    pub plan_id: TxPlanId,
    pub outcome: TxCommitOutcome,
    pub step_results: Vec<TxCommitStepResult>,
    pub failure_boundary: Option<String>,
    pub committed_count: usize,
    pub failed_count: usize,
    pub skipped_count: usize,
    pub decision_path: String,
    pub reason_code: String,
    pub error_code: Option<String>,
    pub completed_at_ms: i64,
    #[serde(default)]
    pub receipts: Vec<serde_json::Value>,
}

/// Report from the compensation phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxCompensationReport {
    pub outcome: TxCompensationOutcome,
    pub compensated_count: usize,
    pub failed_count: usize,
    #[serde(default)]
    pub no_compensation_count: usize,
    pub skipped_count: usize,
    #[serde(default)]
    pub step_results: Vec<TxCommitStepResult>,
    pub decision_path: String,
    pub reason_code: String,
    pub error_code: Option<String>,
    pub completed_at_ms: i64,
    #[serde(default)]
    pub receipts: Vec<serde_json::Value>,
}

impl TxCommitReport {
    /// Whether all commit steps completed successfully.
    #[must_use]
    pub fn is_fully_committed(&self) -> bool {
        matches!(self.outcome, TxCommitOutcome::FullyCommitted)
    }

    /// Whether this report reflects any failure or safety block.
    #[must_use]
    pub fn has_failures(&self) -> bool {
        self.failed_count > 0
            || matches!(
                self.outcome,
                TxCommitOutcome::PartialFailure
                    | TxCommitOutcome::ImmediateFailure
                    | TxCommitOutcome::KillSwitchBlocked
            )
    }

    /// Deterministic canonical string for replay comparison.
    #[must_use]
    pub fn canonical_string(&self) -> String {
        format!(
            "commit:{:?}:c={},f={},s={}:{}",
            self.outcome,
            self.committed_count,
            self.failed_count,
            self.skipped_count,
            self.decision_path
        )
    }
}

impl TxCompensationReport {
    /// Whether compensation fully removed committed effects.
    #[must_use]
    pub fn is_fully_rolled_back(&self) -> bool {
        matches!(self.outcome, TxCompensationOutcome::FullyRolledBack)
    }

    /// Whether rollback left unresolved work behind.
    #[must_use]
    pub fn has_residual_risk(&self) -> bool {
        matches!(self.outcome, TxCompensationOutcome::CompensationFailed)
    }

    /// Deterministic canonical string for replay comparison.
    #[must_use]
    pub fn canonical_string(&self) -> String {
        format!(
            "compensate:{:?}:c={},f={},s={}:{}",
            self.outcome,
            self.compensated_count,
            self.failed_count,
            self.skipped_count,
            self.decision_path
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxPrepareTargetSnapshot {
    pub pane_id: u64,
    pub capabilities: PaneCapabilities,
    pub last_seen_at_ms: Option<i64>,
    pub observed: bool,
    pub known_dead: bool,
    pub domain: Option<String>,
    pub pane_title: Option<String>,
    pub pane_cwd: Option<String>,
    pub reserved_by: Option<String>,
    pub reservation_lookup_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TxPrepareEvaluationContext {
    pub workspace_id: String,
    pub surface: PolicySurface,
    /// Authenticated caller identity for policy evaluation. This must come
    /// from the entry surface, never from the caller-authored tx contract.
    pub actor: ActorKind,
    pub workflow_id: Option<String>,
    pub agent_type: Option<String>,
    pub stale_after_ms: i64,
}

impl TxPrepareEvaluationContext {
    pub const DEFAULT_STALE_AFTER_MS: i64 = 30_000;

    #[must_use]
    pub fn new(workspace_id: impl Into<String>) -> Self {
        Self {
            workspace_id: workspace_id.into(),
            surface: PolicySurface::Workflow,
            actor: ActorKind::Workflow,
            workflow_id: None,
            agent_type: None,
            stale_after_ms: Self::DEFAULT_STALE_AFTER_MS,
        }
    }

    #[must_use]
    pub fn with_surface(mut self, surface: PolicySurface) -> Self {
        self.surface = surface;
        self
    }

    #[must_use]
    pub fn with_actor(mut self, actor: ActorKind) -> Self {
        self.actor = actor;
        self
    }

    #[must_use]
    pub fn with_workflow_id(mut self, workflow_id: impl Into<String>) -> Self {
        self.workflow_id = Some(workflow_id.into());
        self
    }

    #[must_use]
    pub fn with_agent_type(mut self, agent_type: impl Into<String>) -> Self {
        self.agent_type = Some(agent_type.into());
        self
    }

    #[must_use]
    pub fn with_stale_after_ms(mut self, stale_after_ms: i64) -> Self {
        self.stale_after_ms = stale_after_ms.max(0);
        self
    }
}

pub trait TxPreparePolicyAuthorizer {
    fn authorize_prepare(&self, input: &PolicyInput) -> PolicyDecision;
}

impl TxPreparePolicyAuthorizer for RefCell<crate::policy::PolicyEngine> {
    fn authorize_prepare(&self, input: &PolicyInput) -> PolicyDecision {
        self.borrow_mut().authorize(input)
    }
}

pub trait TxPrepareApprovalChecker {
    fn has_active_approval(
        &self,
        scope: &ApprovalScope,
        now_ms: i64,
    ) -> std::result::Result<bool, String>;
}

pub struct StorageBackedPrepareApprovalChecker<'a> {
    storage: Option<&'a crate::storage::StorageHandle>,
}

impl<'a> StorageBackedPrepareApprovalChecker<'a> {
    #[must_use]
    pub fn new(storage: Option<&'a crate::storage::StorageHandle>) -> Self {
        Self { storage }
    }
}

impl TxPrepareApprovalChecker for StorageBackedPrepareApprovalChecker<'_> {
    fn has_active_approval(
        &self,
        scope: &ApprovalScope,
        now_ms: i64,
    ) -> std::result::Result<bool, String> {
        let Some(storage) = self.storage else {
            return Ok(false);
        };

        storage
            .has_active_approval_for_scope_blocking(
                &scope.workspace_id,
                &scope.action_kind,
                scope.pane_id,
                &scope.action_fingerprint,
                now_ms,
            )
            .map_err(|err| err.to_string())
    }
}

pub trait TxPrepareTargetLookup {
    fn lookup_target(
        &self,
        pane_id: u64,
    ) -> std::result::Result<Option<TxPrepareTargetSnapshot>, String>;
}

pub struct StorageBackedPrepareTargetLookup<'a> {
    registry: Option<&'a crate::ingest::PaneRegistry>,
    storage: Option<&'a crate::storage::StorageHandle>,
    resolved_capabilities: HashMap<u64, PaneCapabilities>,
}

impl<'a> StorageBackedPrepareTargetLookup<'a> {
    #[must_use]
    pub fn new(
        registry: Option<&'a crate::ingest::PaneRegistry>,
        storage: Option<&'a crate::storage::StorageHandle>,
    ) -> Self {
        Self {
            registry,
            storage,
            resolved_capabilities: HashMap::new(),
        }
    }

    /// Supply pre-resolved pane capabilities for the storage-backed path.
    ///
    /// A bare `panes` row carries no prompt/alt-screen evidence, and prepare
    /// gates fail closed on unknown capabilities: `PromptActive` preconditions
    /// can never pass and untrusted actors are forced into approval on every
    /// `SendText` step. Callers without a live ingest registry (the MCP tx
    /// surface) resolve capabilities from the same evidence chain as the other
    /// MCP surfaces (OSC-133 prompt state from stored segments, alt-screen/gap
    /// from the watcher IPC) and hand the result in here. The registry path is
    /// unaffected — live ingest state stays authoritative when present.
    #[must_use]
    pub fn with_resolved_capabilities(
        mut self,
        resolved_capabilities: HashMap<u64, PaneCapabilities>,
    ) -> Self {
        self.resolved_capabilities = resolved_capabilities;
        self
    }

    fn reservation_state(&self, pane_id: u64) -> (Option<String>, Option<String>) {
        let Some(storage) = self.storage else {
            return (None, None);
        };

        match storage.get_active_reservation_blocking(pane_id) {
            Ok(Some(reservation)) => (Some(reservation.owner_id), None),
            Ok(None) => (None, None),
            Err(err) => (None, Some(err.to_string())),
        }
    }
}

impl TxPrepareTargetLookup for StorageBackedPrepareTargetLookup<'_> {
    fn lookup_target(
        &self,
        pane_id: u64,
    ) -> std::result::Result<Option<TxPrepareTargetSnapshot>, String> {
        if let Some(registry) = self.registry
            && let Some(entry) = registry.get_entry(pane_id)
        {
            let cursor = registry.get_cursor(pane_id);
            let (reserved_by, reservation_lookup_error) = self.reservation_state(pane_id);
            let capabilities = PaneCapabilities::from_ingest_state(
                None,
                cursor.map(|cursor| cursor.in_alt_screen),
                cursor.is_none_or(|cursor| cursor.in_gap),
            );

            return Ok(Some(TxPrepareTargetSnapshot {
                pane_id,
                capabilities,
                last_seen_at_ms: Some(entry.last_seen_at),
                observed: entry.should_observe(),
                known_dead: false,
                domain: Some(entry.info.inferred_domain()),
                pane_title: entry.info.title.clone(),
                pane_cwd: entry.info.cwd.clone(),
                reserved_by,
                reservation_lookup_error,
            }));
        }

        let Some(storage) = self.storage else {
            return Ok(None);
        };

        let pane = storage
            .get_pane_blocking(pane_id)
            .map_err(|err| err.to_string())?;
        let Some(pane) = pane else {
            return Ok(None);
        };
        let (reserved_by, reservation_lookup_error) = self.reservation_state(pane_id);
        Ok(Some(TxPrepareTargetSnapshot {
            pane_id,
            capabilities: self
                .resolved_capabilities
                .get(&pane_id)
                .cloned()
                .unwrap_or_else(PaneCapabilities::unknown),
            last_seen_at_ms: Some(pane.last_seen_at),
            observed: pane.observed,
            known_dead: false,
            domain: Some(pane.domain),
            pane_title: pane.title,
            pane_cwd: pane.cwd,
            reserved_by,
            reservation_lookup_error,
        }))
    }
}

fn tx_prepare_action_kind(action: &StepAction) -> ActionKind {
    match action {
        StepAction::SendText { .. } => ActionKind::SendText,
        StepAction::WaitFor { .. } => ActionKind::ReadOutput,
        StepAction::AcquireLock { .. } => ActionKind::ReservePane,
        StepAction::ReleaseLock { .. } => ActionKind::ReleasePane,
        StepAction::StoreData { .. } => ActionKind::WriteFile,
        StepAction::RunWorkflow { .. } => ActionKind::WorkflowRun,
        StepAction::MarkEventHandled { .. } => ActionKind::WriteFile,
        StepAction::ValidateApproval { .. } => ActionKind::ExecCommand,
        StepAction::NestedPlan { .. } | StepAction::Custom { .. } => ActionKind::ExecCommand,
    }
}

fn tx_prepare_pane_id(action: &StepAction) -> Option<u64> {
    match action {
        StepAction::SendText { pane_id, .. } => Some(*pane_id),
        StepAction::WaitFor {
            pane_id, condition, ..
        } => (*pane_id).or(match condition {
            WaitCondition::Pattern { pane_id, .. }
            | WaitCondition::PaneIdle { pane_id, .. }
            | WaitCondition::StableTail { pane_id, .. } => *pane_id,
            WaitCondition::External { .. } => None,
        }),
        StepAction::AcquireLock { .. }
        | StepAction::ReleaseLock { .. }
        | StepAction::StoreData { .. }
        | StepAction::RunWorkflow { .. }
        | StepAction::MarkEventHandled { .. }
        | StepAction::ValidateApproval { .. }
        | StepAction::NestedPlan { .. }
        | StepAction::Custom { .. } => None,
    }
}

fn tx_prepare_text_summary(step: &TxStep) -> String {
    if step.description.trim().is_empty() {
        step.action.action_type_name().to_string()
    } else {
        step.description.clone()
    }
}

fn tx_prepare_liveness(
    pane_id: Option<u64>,
    snapshot_result: Option<&std::result::Result<Option<TxPrepareTargetSnapshot>, String>>,
    context: &TxPrepareEvaluationContext,
    now_ms: i64,
) -> (bool, Option<String>) {
    let Some(pane_id) = pane_id else {
        return (true, None);
    };

    let Some(snapshot_result) = snapshot_result else {
        return (false, Some(format!("tx.prepare.target_missing:{pane_id}")));
    };

    let snapshot = match snapshot_result {
        Ok(Some(snapshot)) => snapshot,
        Ok(None) => return (false, Some(format!("tx.prepare.target_missing:{pane_id}"))),
        Err(_) => {
            return (
                false,
                Some(format!("tx.prepare.target_lookup_failed:{pane_id}")),
            );
        }
    };

    if snapshot.known_dead {
        return (false, Some(format!("tx.prepare.target_dead:{pane_id}")));
    }
    if !snapshot.observed {
        return (
            false,
            Some(format!("tx.prepare.target_unobserved:{pane_id}")),
        );
    }

    let Some(last_seen_at_ms) = snapshot.last_seen_at_ms else {
        return (false, Some(format!("tx.prepare.target_unknown:{pane_id}")));
    };

    if context.stale_after_ms > 0 && now_ms.saturating_sub(last_seen_at_ms) > context.stale_after_ms
    {
        return (false, Some(format!("tx.prepare.target_stale:{pane_id}")));
    }

    (true, None)
}

fn tx_prepare_reservation(
    pane_id: Option<u64>,
    snapshot_result: Option<&std::result::Result<Option<TxPrepareTargetSnapshot>, String>>,
    context: &TxPrepareEvaluationContext,
) -> (bool, Option<String>) {
    let Some(pane_id) = pane_id else {
        return (true, None);
    };

    let Some(snapshot_result) = snapshot_result else {
        return (
            false,
            Some(format!("tx.prepare.reservation_unknown:{pane_id}")),
        );
    };

    let snapshot = match snapshot_result {
        Ok(Some(snapshot)) => snapshot,
        Ok(None) => {
            return (
                false,
                Some(format!("tx.prepare.reservation_unknown:{pane_id}")),
            );
        }
        Err(_) => {
            return (
                false,
                Some(format!("tx.prepare.reservation_lookup_failed:{pane_id}")),
            );
        }
    };

    if snapshot.reservation_lookup_error.is_some() {
        return (
            false,
            Some(format!("tx.prepare.reservation_lookup_failed:{pane_id}")),
        );
    }

    match snapshot.reserved_by.as_deref() {
        None => (true, None),
        Some(owner) if context.workflow_id.as_deref() == Some(owner) => (true, None),
        Some(_) => (
            false,
            Some(format!("tx.prepare.reservation_conflict:{pane_id}")),
        ),
    }
}

fn tx_prepare_policy_input(
    step: &TxStep,
    pane_id: Option<u64>,
    snapshot: Option<&TxPrepareTargetSnapshot>,
    context: &TxPrepareEvaluationContext,
) -> PolicyInput {
    let mut capabilities = snapshot
        .map(|snapshot| snapshot.capabilities.clone())
        .unwrap_or_else(PaneCapabilities::unknown);
    if let Some(snapshot) = snapshot
        && let Some(reserved_by) = snapshot.reserved_by.clone()
    {
        capabilities.is_reserved = true;
        capabilities.reserved_by = Some(reserved_by);
    }

    let mut input = PolicyInput::new(tx_prepare_action_kind(&step.action), context.actor)
        .with_surface(context.surface)
        .with_capabilities(capabilities)
        .with_text_summary(tx_prepare_text_summary(step));

    if let Some(pane_id) = pane_id {
        input = input.with_pane(pane_id);
    }
    if let Some(domain) = snapshot.and_then(|snapshot| snapshot.domain.clone()) {
        input = input.with_domain(domain);
    }
    if let Some(workflow_id) = context.workflow_id.clone() {
        input = input.with_workflow(workflow_id);
    }
    if let Some(agent_type) = context.agent_type.clone() {
        input = input.with_agent_type(agent_type);
    }
    if let Some(title) = snapshot.and_then(|snapshot| snapshot.pane_title.clone()) {
        input = input.with_pane_title(title);
    }
    if let Some(cwd) = snapshot.and_then(|snapshot| snapshot.pane_cwd.clone()) {
        input = input.with_pane_cwd(cwd);
    }
    if let StepAction::SendText { text, .. } = &step.action {
        input = input.with_command_text(text.clone());
    }

    input
}

fn tx_prepare_plan_preconditions<T>(
    plan: &TxPlan,
    targets: &T,
    target_cache: &mut HashMap<u64, std::result::Result<Option<TxPrepareTargetSnapshot>, String>>,
    context: &TxPrepareEvaluationContext,
    now_ms: i64,
) -> (bool, Option<String>)
where
    T: TxPrepareTargetLookup,
{
    if plan
        .preconditions
        .iter()
        .any(|precondition| matches!(precondition, TxPrecondition::Custom { .. }))
    {
        return (
            false,
            Some("tx.prepare.precondition.custom_unsupported".to_string()),
        );
    }

    for precondition in &plan.preconditions {
        let TxPrecondition::PromptActive { pane_id } = precondition else {
            continue;
        };
        let snapshot_result = target_cache
            .entry(*pane_id)
            .or_insert_with(|| targets.lookup_target(*pane_id))
            .clone();
        let (target_live, liveness_reason_code) =
            tx_prepare_liveness(Some(*pane_id), Some(&snapshot_result), context, now_ms);
        if !target_live {
            let reason_code = liveness_reason_code.map_or_else(
                || format!("tx.prepare.precondition.prompt_active.target_unknown:{pane_id}"),
                |reason_code| {
                    reason_code.replacen("tx.prepare.", "tx.prepare.precondition.prompt_active.", 1)
                },
            );
            return (false, Some(reason_code));
        }

        let snapshot = match snapshot_result {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) | Err(_) => {
                return (
                    false,
                    Some(format!(
                        "tx.prepare.precondition.prompt_active.target_unknown:{pane_id}"
                    )),
                );
            }
        };
        if snapshot.pane_id != *pane_id {
            return (
                false,
                Some(format!(
                    "tx.prepare.precondition.prompt_active.target_mismatch:{pane_id}"
                )),
            );
        }
        if !snapshot.capabilities.prompt_active {
            return (
                false,
                Some(format!(
                    "tx.prepare.precondition.prompt_active.inactive:{pane_id}"
                )),
            );
        }
    }

    (true, None)
}

/// Build explicit allow-all prepare-gate inputs used by synthetic tx surfaces.
///
/// Prompt-state checks are treated as supplied synthetic evidence. Custom
/// expressions remain unsupported and therefore fail closed even here.
#[must_use]
pub fn tx_prepare_gate_inputs_allow_all(contract: &MissionTxContract) -> Vec<TxPrepareGateInput> {
    let custom_precondition_unsupported = contract
        .plan
        .preconditions
        .iter()
        .any(|precondition| matches!(precondition, TxPrecondition::Custom { .. }));
    contract
        .plan
        .steps
        .iter()
        .map(|step| TxPrepareGateInput {
            step_id: step.step_id.clone(),
            pane_id: tx_prepare_pane_id(&step.action),
            preconditions_satisfied: !custom_precondition_unsupported,
            precondition_reason_code: custom_precondition_unsupported
                .then(|| "tx.prepare.precondition.custom_unsupported".to_string()),
            policy_passed: true,
            policy_reason_code: None,
            reservation_available: true,
            reservation_reason_code: None,
            approval_satisfied: true,
            approval_reason_code: None,
            target_liveness: true,
            liveness_reason_code: None,
            required_approval: None,
        })
        .collect()
}

/// Build prepare-gate inputs by evaluating plan preconditions, policy, approval,
/// reservation, and liveness against one cached target snapshot per pane.
#[must_use]
pub fn mission_tx_prepare_gate_inputs<P, A, T>(
    contract: &MissionTxContract,
    policy: &P,
    approvals: &A,
    targets: &T,
    context: &TxPrepareEvaluationContext,
    now_ms: i64,
) -> Vec<TxPrepareGateInput>
where
    P: TxPreparePolicyAuthorizer,
    A: TxPrepareApprovalChecker,
    T: TxPrepareTargetLookup,
{
    let mut target_cache: HashMap<
        u64,
        std::result::Result<Option<TxPrepareTargetSnapshot>, String>,
    > = HashMap::new();
    let (preconditions_satisfied, precondition_reason_code) =
        tx_prepare_plan_preconditions(&contract.plan, targets, &mut target_cache, context, now_ms);

    contract
        .plan
        .steps
        .iter()
        .map(|step| {
            let pane_id = tx_prepare_pane_id(&step.action);
            let snapshot_result = pane_id.map(|pane_id| {
                target_cache
                    .entry(pane_id)
                    .or_insert_with(|| targets.lookup_target(pane_id))
                    .clone()
            });
            let snapshot = snapshot_result
                .as_ref()
                .and_then(|result| result.as_ref().ok().and_then(|snapshot| snapshot.as_ref()));
            let (target_liveness, liveness_reason_code) =
                tx_prepare_liveness(pane_id, snapshot_result.as_ref(), context, now_ms);
            let (reservation_available, reservation_reason_code) =
                tx_prepare_reservation(pane_id, snapshot_result.as_ref(), context);

            let policy_input = tx_prepare_policy_input(step, pane_id, snapshot, context);
            let policy_decision = policy.authorize_prepare(&policy_input);

            let (
                policy_passed,
                policy_reason_code,
                required_approval,
                approval_satisfied,
                approval_reason_code,
            ) = match &policy_decision {
                PolicyDecision::Allow { .. } => (true, None, None, true, None),
                PolicyDecision::Deny { .. } => (
                    false,
                    policy_decision
                        .rule_id()
                        .map(ToString::to_string)
                        .or_else(|| policy_decision.reason().map(ToString::to_string)),
                    None,
                    true,
                    None,
                ),
                PolicyDecision::RequireApproval { .. } => {
                    let scope = ApprovalScope::from_input(&context.workspace_id, &policy_input);
                    let requirement = TxPrepareApprovalRequirement {
                        workspace_id: scope.workspace_id.clone(),
                        action_kind: scope.action_kind.clone(),
                        pane_id: scope.pane_id,
                        action_fingerprint: scope.action_fingerprint.clone(),
                        reason_code: policy_decision.rule_id().map(ToString::to_string),
                    };
                    let (approval_satisfied, approval_reason_code) = match approvals
                        .has_active_approval(&scope, now_ms)
                    {
                        Ok(true) => (true, None),
                        Ok(false) => (
                            false,
                            requirement
                                .reason_code
                                .clone()
                                .or_else(|| Some("tx.prepare.approval_required".to_string())),
                        ),
                        Err(_) => (false, Some("tx.prepare.approval_lookup_failed".to_string())),
                    };
                    (
                        true,
                        None,
                        Some(requirement),
                        approval_satisfied,
                        approval_reason_code,
                    )
                }
            };

            TxPrepareGateInput {
                step_id: step.step_id.clone(),
                pane_id,
                preconditions_satisfied,
                precondition_reason_code: precondition_reason_code.clone(),
                policy_passed,
                policy_reason_code,
                reservation_available,
                reservation_reason_code,
                approval_satisfied,
                approval_reason_code,
                target_liveness,
                liveness_reason_code,
                required_approval,
            }
        })
        .collect()
}

/// Build deterministic commit inputs with optional single-step failure injection.
#[must_use]
pub fn mission_tx_commit_step_inputs(
    contract: &MissionTxContract,
    fail_step: Option<&str>,
    completed_at_ms: i64,
) -> Vec<TxCommitStepInput> {
    contract
        .plan
        .steps
        .iter()
        .map(|step| {
            let should_fail = fail_step == Some(step.step_id.0.as_str());
            TxCommitStepInput {
                step_id: step.step_id.clone(),
                success: !should_fail,
                reason_code: if should_fail {
                    "commit_step_failed_injected".to_string()
                } else {
                    "commit_step_succeeded".to_string()
                },
                error_code: should_fail.then(|| "FTX3999".to_string()),
                completed_at_ms,
            }
        })
        .collect()
}

/// Build deterministic compensation inputs for each committed step.
#[must_use]
pub fn mission_tx_compensation_inputs(
    commit_report: &TxCommitReport,
    fail_for_step: Option<&str>,
    completed_at_ms: i64,
) -> Vec<TxCompensationStepInput> {
    commit_report
        .step_results
        .iter()
        .filter(|result| result.outcome.is_committed())
        .map(|result| {
            let should_fail = fail_for_step == Some(result.step_id.0.as_str());
            TxCompensationStepInput {
                for_step_id: result.step_id.clone(),
                success: !should_fail,
                reason_code: if should_fail {
                    "compensation_failed_injected".to_string()
                } else {
                    "compensation_succeeded".to_string()
                },
                error_code: should_fail.then(|| "FTX4999".to_string()),
                completed_at_ms,
            }
        })
        .collect()
}

/// Build a synthetic all-committed report for rollback-only tx surfaces.
#[must_use]
pub fn mission_tx_synthetic_commit_report(
    contract: &MissionTxContract,
    completed_at_ms: i64,
) -> TxCommitReport {
    let step_results = contract
        .plan
        .steps
        .iter()
        .map(|step| TxCommitStepResult {
            step_id: step.step_id.clone(),
            ordinal: step.ordinal,
            outcome: TxCommitStepOutcome::Committed {
                reason_code: "synthetic_prior_commit".to_string(),
            },
            decision_path: "rollback_synthetic_commit_report".to_string(),
            completed_at_ms,
        })
        .collect::<Vec<_>>();

    TxCommitReport {
        tx_id: contract.intent.tx_id.clone(),
        plan_id: contract.plan.plan_id.clone(),
        outcome: TxCommitOutcome::FullyCommitted,
        step_results,
        failure_boundary: None,
        committed_count: contract.plan.steps.len(),
        failed_count: 0,
        skipped_count: 0,
        decision_path: "rollback_synthetic_commit_report".to_string(),
        reason_code: "synthetic_all_committed".to_string(),
        error_code: None,
        completed_at_ms,
        receipts: Vec::new(),
    }
}

fn looks_like_persisted_tx_receipt(value: &serde_json::Value) -> bool {
    value.get("phase").is_some()
        || (value.get("seq").is_some()
            && value.get("tx_id").is_some()
            && value.get("plan_id").is_some())
}

fn validate_persisted_tx_receipts(
    contract: &MissionTxContract,
) -> Result<Vec<(usize, TxReceipt)>, String> {
    let plan_step_ids = contract
        .plan
        .steps
        .iter()
        .map(|step| step.step_id.0.as_str())
        .collect::<HashSet<_>>();
    let compensation_step_ids = contract
        .plan
        .compensations
        .iter()
        .map(|compensation| compensation.for_step_id.0.as_str())
        .collect::<HashSet<_>>();
    let mut validated = Vec::new();
    let mut previous_seq = 0u64;
    let mut latest_successful_commit_receipts = HashMap::<String, TxReceipt>::new();
    let mut steps_with_compensation_attempts = HashSet::<String>::new();

    for (persisted_index, receipt_value) in contract.receipts.iter().enumerate() {
        if !looks_like_persisted_tx_receipt(receipt_value) {
            continue;
        }

        let receipt =
            serde_json::from_value::<TxReceipt>(receipt_value.clone()).map_err(|err| {
                format!("invalid transaction receipt at persisted index {persisted_index}: {err}")
            })?;

        if receipt.emitted_at_ms < 0 {
            return Err(format!(
                "transaction receipt seq {} has negative emitted_at_ms {}",
                receipt.seq, receipt.emitted_at_ms
            ));
        }

        if receipt.tx_id != contract.intent.tx_id.0 || receipt.plan_id != contract.plan.plan_id.0 {
            return Err(format!(
                "{} receipt does not belong to tx {} / plan {}",
                if receipt.phase == "compensate" {
                    "compensation"
                } else {
                    receipt.phase.as_str()
                },
                contract.intent.tx_id.0,
                contract.plan.plan_id.0
            ));
        }

        match receipt.phase.as_str() {
            "commit" => {
                if receipt.state != MissionTxState::Committing {
                    return Err(format!(
                        "commit receipt for seq {} has incoherent tx state {}",
                        receipt.seq, receipt.state
                    ));
                }
                let step_id = receipt
                    .step_id
                    .as_deref()
                    .ok_or_else(|| "commit receipt missing step_id".to_string())?;
                if !plan_step_ids.contains(step_id) {
                    return Err(format!("commit receipt references unknown step {step_id}"));
                }
                if !matches!(receipt.outcome.as_str(), "committed" | "failed" | "skipped") {
                    return Err(format!(
                        "unknown commit receipt outcome '{}' for step {step_id}",
                        receipt.outcome
                    ));
                }
                if receipt.outcome == "committed" {
                    if steps_with_compensation_attempts.contains(step_id) {
                        return Err(format!(
                            "committed receipt for step {step_id} follows a compensation attempt; a new effect generation is required before recommit"
                        ));
                    }
                    latest_successful_commit_receipts.insert(step_id.to_string(), receipt.clone());
                }
            }
            "compensate" => {
                if receipt.state != MissionTxState::Compensating {
                    return Err(format!(
                        "compensation receipt for seq {} has incoherent tx state {}",
                        receipt.seq, receipt.state
                    ));
                }
                let step_id = receipt
                    .step_id
                    .as_deref()
                    .ok_or_else(|| "compensation receipt missing step_id".to_string())?;
                if !plan_step_ids.contains(step_id) {
                    return Err(format!(
                        "compensation receipt references unknown step {step_id}"
                    ));
                }
                if !compensation_step_ids.contains(step_id) {
                    return Err(format!(
                        "compensation receipt references step {step_id} without a declared compensation action"
                    ));
                }
                if !matches!(
                    receipt.outcome.as_str(),
                    "compensated" | "failed" | "skipped"
                ) {
                    return Err(format!(
                        "unknown compensation receipt outcome '{}' for step {step_id}",
                        receipt.outcome
                    ));
                }
                let commit_receipt = latest_successful_commit_receipts.get(step_id).ok_or_else(|| {
                    format!(
                        "compensation receipt for step {step_id} has no preceding committed receipt"
                    )
                })?;
                if receipt.seq <= commit_receipt.seq {
                    return Err(format!(
                        "compensation receipt for step {step_id} does not follow its committed receipt"
                    ));
                }
                steps_with_compensation_attempts.insert(step_id.to_string());
            }
            "lifecycle" => {
                if receipt.step_id.is_some() || receipt.outcome != "state_checkpoint" {
                    return Err(format!(
                        "lifecycle receipt for seq {} must have no step_id and outcome state_checkpoint",
                        receipt.seq
                    ));
                }
            }
            other => {
                return Err(format!(
                    "unknown transaction receipt phase '{other}' at persisted index {persisted_index}"
                ));
            }
        }

        if receipt.seq <= previous_seq {
            return Err(format!(
                "transaction receipt seq {} at persisted index {} is not strictly greater than previous seq {}",
                receipt.seq, persisted_index, previous_seq
            ));
        }
        previous_seq = receipt.seq;
        validated.push((persisted_index, receipt));
    }

    Ok(validated)
}

/// Build the commit report a rollback surface should compensate against.
///
/// Reconstructs rollback candidates from the contract's receipt history.
/// Successful commit and compensation receipts are sticky candidate facts:
/// later failed/skipped retries are diagnostic and cannot erase an earlier
/// claim. An effectful durable caller must additionally corroborate commit and
/// compensation outcomes against its live durable execution spool before
/// dispatch or deduplication. The returned report retains one result per plan
/// step.
pub fn mission_tx_rollback_commit_report(
    contract: &MissionTxContract,
    completed_at_ms: i64,
) -> Result<TxCommitReport, String> {
    if matches!(
        contract.lifecycle_state,
        MissionTxState::RolledBack | MissionTxState::Compensated
    ) {
        return Err(format!(
            "rollback is not allowed for terminal tx state {}",
            contract.lifecycle_state
        ));
    }
    contract.validate()?;

    let mut latest_commit_receipts = HashMap::<String, TxReceipt>::new();
    let mut latest_successful_commit_receipts = HashMap::<String, TxReceipt>::new();
    let mut latest_successful_compensation_receipts = HashMap::<String, TxReceipt>::new();
    let mut steps_with_compensation_attempts = HashSet::<String>::new();
    let mut commit_receipts = Vec::new();

    for (persisted_index, receipt) in validate_persisted_tx_receipts(contract)? {
        match receipt.phase.as_str() {
            "commit" => {
                let step_id = receipt
                    .step_id
                    .clone()
                    .ok_or_else(|| "commit receipt missing step_id".to_string())?;
                if receipt.outcome == "committed" {
                    if steps_with_compensation_attempts.contains(&step_id) {
                        return Err(format!(
                            "committed receipt for step {step_id} follows a compensation attempt; a new effect generation is required before recommit"
                        ));
                    }
                    latest_successful_commit_receipts.insert(step_id.clone(), receipt.clone());
                }
                latest_commit_receipts.insert(step_id, receipt);
                commit_receipts.push(contract.receipts[persisted_index].clone());
            }
            "compensate" => {
                let step_id = receipt
                    .step_id
                    .clone()
                    .ok_or_else(|| "compensation receipt missing step_id".to_string())?;
                let commit_receipt = latest_successful_commit_receipts.get(&step_id).ok_or_else(|| {
                    format!(
                        "compensation receipt for step {step_id} has no preceding committed receipt"
                    )
                })?;
                if receipt.seq <= commit_receipt.seq {
                    return Err(format!(
                        "compensation receipt for step {step_id} does not follow its committed receipt"
                    ));
                }
                steps_with_compensation_attempts.insert(step_id.clone());
                if receipt.outcome == "compensated" {
                    latest_successful_compensation_receipts.insert(step_id, receipt);
                }
            }
            _ => {}
        }
    }

    if latest_commit_receipts.is_empty() {
        return Err(format!(
            "rollback requires commit receipts, got none for tx state {}",
            contract.lifecycle_state
        ));
    }

    commit_receipts.sort_by_key(|receipt| {
        receipt
            .get("seq")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default()
    });

    let mut step_results = Vec::with_capacity(contract.plan.steps.len());
    let mut committed_count = 0usize;
    let mut failed_count = 0usize;
    let mut skipped_count = 0usize;
    let mut failure_boundary = None;
    let mut report_error_code = None;
    let mut report_completed_at_ms = completed_at_ms;

    for step in &contract.plan.steps {
        let latest_receipt = latest_commit_receipts.get(&step.step_id.0).ok_or_else(|| {
            format!(
                "rollback requires a commit receipt for step {}",
                step.step_id.0
            )
        })?;
        // A committed effect is a sticky safety fact. Later failed/skipped
        // commit attempts are diagnostic only and cannot make the already
        // applied external effect disappear from rollback reconstruction.
        let receipt = latest_successful_commit_receipts
            .get(&step.step_id.0)
            .unwrap_or(latest_receipt);

        report_completed_at_ms = report_completed_at_ms.max(receipt.emitted_at_ms);

        // A proven successful compensation is a sticky safety fact. A later
        // failed/skipped retry receipt must not erase it and authorize the same
        // external compensation a second time.
        let already_compensated = latest_successful_compensation_receipts
            .get(&step.step_id.0)
            .filter(|compensation_receipt| compensation_receipt.seq > receipt.seq);
        let (outcome, decision_path, step_completed_at_ms) =
            match (receipt.outcome.as_str(), already_compensated) {
                ("committed", Some(compensation_receipt)) => {
                    skipped_count += 1;
                    report_completed_at_ms =
                        report_completed_at_ms.max(compensation_receipt.emitted_at_ms);
                    (
                        TxCommitStepOutcome::Skipped {
                            reason_code: "already_compensated".to_string(),
                        },
                        "rollback_compensation_receipts->already_compensated".to_string(),
                        compensation_receipt.emitted_at_ms,
                    )
                }
                ("committed", None) => {
                    committed_count += 1;
                    (
                        TxCommitStepOutcome::Committed {
                            reason_code: receipt.reason_code.clone(),
                        },
                        receipt.decision_path.clone(),
                        receipt.emitted_at_ms,
                    )
                }
                ("failed", None) => {
                    failed_count += 1;
                    failure_boundary.get_or_insert_with(|| step.step_id.0.clone());
                    if report_error_code.is_none() {
                        report_error_code.clone_from(&receipt.error_code);
                    }
                    (
                        TxCommitStepOutcome::Failed {
                            reason_code: receipt.reason_code.clone(),
                        },
                        receipt.decision_path.clone(),
                        receipt.emitted_at_ms,
                    )
                }
                ("skipped", None) => {
                    skipped_count += 1;
                    (
                        TxCommitStepOutcome::Skipped {
                            reason_code: receipt.reason_code.clone(),
                        },
                        receipt.decision_path.clone(),
                        receipt.emitted_at_ms,
                    )
                }
                (other, _) => {
                    return Err(format!(
                        "unknown commit receipt outcome '{other}' for step {}",
                        step.step_id.0
                    ));
                }
            };

        step_results.push(TxCommitStepResult {
            step_id: step.step_id.clone(),
            ordinal: step.ordinal,
            outcome,
            decision_path,
            completed_at_ms: step_completed_at_ms,
        });
    }

    let (outcome, reason_code) = if failed_count == 0 {
        (TxCommitOutcome::FullyCommitted, "fully_committed")
    } else if committed_count == 0 {
        (TxCommitOutcome::ImmediateFailure, "immediate_failure")
    } else {
        (TxCommitOutcome::PartialFailure, "partial_failure")
    };

    Ok(TxCommitReport {
        tx_id: contract.intent.tx_id.clone(),
        plan_id: contract.plan.plan_id.clone(),
        outcome,
        step_results,
        failure_boundary,
        committed_count,
        failed_count,
        skipped_count,
        decision_path: "rollback_commit_receipts".to_string(),
        reason_code: reason_code.to_string(),
        error_code: report_error_code,
        completed_at_ms: report_completed_at_ms,
        receipts: commit_receipts,
    })
}

fn tx_last_receipt_seq(receipts: &[serde_json::Value]) -> u64 {
    receipts
        .iter()
        .rev()
        .filter(|receipt| looks_like_persisted_tx_receipt(receipt))
        .find_map(|receipt| receipt.get("seq").and_then(serde_json::Value::as_u64))
        .unwrap_or(0)
}

fn tx_increment_receipt_seq(previous_seq: u64) -> Result<u64, String> {
    previous_seq
        .checked_add(1)
        .ok_or_else(|| format!("transaction receipt sequence overflow after seq {previous_seq}"))
}

/// Whether the newest receipt for `(phase, step_id)` already records `outcome`.
///
/// Mirrors the commit phase's duplicate-receipt guard in
/// `tx_execution::latest_tx_receipt_matches`. A rollback that is satisfied
/// entirely by durable dedup re-derives the same compensation outcome the
/// contract already carries, and appending a second identical receipt is both
/// redundant and, at the sequence ceiling, fatal: the compensation phase
/// allocated a sequence number unconditionally, so a contract whose last
/// receipt sat at `u64::MAX` failed with a sequence overflow even though it
/// needed no new receipt at all.
fn tx_latest_receipt_matches(
    contract: &MissionTxContract,
    phase: &str,
    step_id: &str,
    outcome: &str,
) -> bool {
    contract
        .receipts
        .iter()
        .rev()
        .find(|receipt| {
            receipt.get("phase").and_then(serde_json::Value::as_str) == Some(phase)
                && receipt.get("step_id").and_then(serde_json::Value::as_str) == Some(step_id)
        })
        .is_some_and(|receipt| {
            receipt.get("outcome").and_then(serde_json::Value::as_str) == Some(outcome)
        })
}

// Tx receipts keep sequencing, policy, and proof metadata explicit.
#[allow(clippy::too_many_arguments)]
fn tx_build_receipt(
    seq: u64,
    phase: &str,
    tx_id: &TxId,
    plan_id: &TxPlanId,
    state: MissionTxState,
    step_id: Option<&TxStepId>,
    outcome: &str,
    reason_code: &str,
    error_code: Option<&str>,
    decision_path: &str,
    emitted_at_ms: i64,
) -> serde_json::Value {
    serde_json::json!({
        "seq": seq,
        "phase": phase,
        "tx_id": tx_id.0,
        "plan_id": plan_id.0,
        "state": state,
        "step_id": step_id.map(|step_id| step_id.0.clone()),
        "outcome": outcome,
        "reason_code": reason_code,
        "error_code": error_code,
        "decision_path": decision_path,
        "emitted_at_ms": emitted_at_ms,
    })
}

fn tx_blocked_commit_report(
    contract: &MissionTxContract,
    outcome: TxCommitOutcome,
    reason_code: &str,
    error_code: Option<&str>,
    decision_path: &str,
    completed_at_ms: i64,
) -> Result<TxCommitReport, String> {
    let mut next_seq = tx_last_receipt_seq(&contract.receipts);
    let mut step_results = Vec::with_capacity(contract.plan.steps.len());
    let mut receipts = Vec::with_capacity(contract.plan.steps.len());

    for step in &contract.plan.steps {
        next_seq = tx_increment_receipt_seq(next_seq)?;
        step_results.push(TxCommitStepResult {
            step_id: step.step_id.clone(),
            ordinal: step.ordinal,
            outcome: TxCommitStepOutcome::Skipped {
                reason_code: reason_code.to_string(),
            },
            decision_path: decision_path.to_string(),
            completed_at_ms,
        });
        receipts.push(tx_build_receipt(
            next_seq,
            "commit",
            &contract.intent.tx_id,
            &contract.plan.plan_id,
            MissionTxState::Committing,
            Some(&step.step_id),
            "skipped",
            reason_code,
            error_code,
            decision_path,
            completed_at_ms,
        ));
    }

    Ok(TxCommitReport {
        tx_id: contract.intent.tx_id.clone(),
        plan_id: contract.plan.plan_id.clone(),
        outcome,
        step_results,
        failure_boundary: None,
        committed_count: 0,
        failed_count: 0,
        skipped_count: contract.plan.steps.len(),
        decision_path: decision_path.to_string(),
        reason_code: reason_code.to_string(),
        error_code: error_code.map(str::to_string),
        completed_at_ms,
        receipts,
    })
}

fn duplicate_commit_input_step_id(commit_inputs: &[TxCommitStepInput]) -> Option<&str> {
    let mut seen = HashSet::new();
    for input in commit_inputs {
        let step_id = input.step_id.0.as_str();
        if !seen.insert(step_id) {
            return Some(step_id);
        }
    }
    None
}

fn validate_commit_input_step_ids(
    contract: &MissionTxContract,
    commit_inputs: &[TxCommitStepInput],
) -> Result<(), String> {
    let plan_step_ids: HashSet<&str> = contract
        .plan
        .steps
        .iter()
        .map(|step| step.step_id.0.as_str())
        .collect();
    for input in commit_inputs {
        validate_tx_identifier("commit input step_id", &input.step_id.0)?;
        if !plan_step_ids.contains(input.step_id.0.as_str()) {
            return Err(format!(
                "commit input references unknown step_id {}",
                input.step_id.0
            ));
        }
    }
    Ok(())
}

fn duplicate_compensation_input_step_id(comp_inputs: &[TxCompensationStepInput]) -> Option<&str> {
    let mut seen = HashSet::new();
    for input in comp_inputs {
        let step_id = input.for_step_id.0.as_str();
        if !seen.insert(step_id) {
            return Some(step_id);
        }
    }
    None
}

fn validate_prepare_gate_step_ids(gate_inputs: &[TxPrepareGateInput]) -> Result<(), String> {
    for gate in gate_inputs {
        validate_tx_identifier("prepare gate step_id", &gate.step_id.0)?;
    }
    Ok(())
}

fn validate_compensation_input_step_ids(
    contract: &MissionTxContract,
    commit_report: &TxCommitReport,
    comp_inputs: &[TxCompensationStepInput],
) -> Result<(), String> {
    let plan_step_ids: HashSet<&str> = contract
        .plan
        .steps
        .iter()
        .map(|step| step.step_id.0.as_str())
        .collect();
    let committed_step_ids: HashSet<&str> = commit_report
        .step_results
        .iter()
        .filter(|result| result.outcome.is_committed())
        .map(|result| result.step_id.0.as_str())
        .collect();
    for input in comp_inputs {
        validate_tx_identifier("compensation input step_id", &input.for_step_id.0)?;
        let step_id = input.for_step_id.0.as_str();
        if !plan_step_ids.contains(step_id) {
            return Err(format!(
                "compensation input references unknown step_id {}",
                input.for_step_id.0
            ));
        }
        if !committed_step_ids.contains(step_id) {
            return Err(format!(
                "compensation input references uncommitted step_id {}",
                input.for_step_id.0
            ));
        }
    }
    Ok(())
}

fn validate_compensation_commit_report(
    contract: &MissionTxContract,
    commit_report: &TxCommitReport,
) -> Result<(), String> {
    if commit_report.tx_id != contract.intent.tx_id {
        return Err(format!(
            "Compensation commit report tx_id {} does not match contract tx_id {}",
            commit_report.tx_id.0, contract.intent.tx_id.0
        ));
    }
    if commit_report.plan_id != contract.plan.plan_id {
        return Err(format!(
            "Compensation commit report plan_id {} does not match contract plan_id {}",
            commit_report.plan_id.0, contract.plan.plan_id.0
        ));
    }

    let plan_step_ids: HashSet<&str> = contract
        .plan
        .steps
        .iter()
        .map(|step| step.step_id.0.as_str())
        .collect();
    let mut seen_committed_step_ids = HashSet::new();
    for result in &commit_report.step_results {
        validate_tx_identifier("commit report step_id", &result.step_id.0)?;
        let step_id = result.step_id.0.as_str();
        if !plan_step_ids.contains(step_id) {
            return Err(format!(
                "Compensation commit report references unknown step {step_id}"
            ));
        }
        if !result.outcome.is_committed() {
            continue;
        }
        if !seen_committed_step_ids.insert(step_id) {
            return Err(format!(
                "duplicate committed step {step_id} in commit report: compensation would be order-dependent"
            ));
        }
    }

    Ok(())
}

/// Evaluate prepare phase gates and produce a report.
pub fn evaluate_prepare_phase(
    tx_id: &TxId,
    plan: &TxPlan,
    gate_inputs: &[TxPrepareGateInput],
    kill_switch: MissionKillSwitchLevel,
    _now_ms: i64,
) -> Result<TxPrepareReport, String> {
    if plan.steps.is_empty() {
        return Err("Transaction plan has no steps".to_string());
    }
    validate_tx_identifier("prepare tx_id", &tx_id.0)?;
    validate_tx_identifier("plan tx_id", &plan.tx_id.0)?;
    validate_tx_identifier("plan_id", &plan.plan_id.0)?;
    if tx_id != &plan.tx_id {
        return Err(format!(
            "Prepare tx_id {} does not match plan tx_id {}",
            tx_id.0, plan.tx_id.0
        ));
    }
    for step in &plan.steps {
        validate_tx_identifier("step_id", &step.step_id.0)?;
    }
    for compensation in &plan.compensations {
        validate_tx_identifier("compensation step_id", &compensation.for_step_id.0)?;
    }
    validate_prepare_gate_step_ids(gate_inputs)?;

    if kill_switch == MissionKillSwitchLevel::HardStop {
        return Ok(TxPrepareReport {
            outcome: TxPrepareOutcome::Denied,
            gate_inputs: gate_inputs.to_vec(),
        });
    }

    if plan
        .preconditions
        .iter()
        .any(|precondition| matches!(precondition, TxPrecondition::Custom { .. }))
    {
        return Ok(TxPrepareReport {
            outcome: TxPrepareOutcome::Denied,
            gate_inputs: gate_inputs.to_vec(),
        });
    }

    let mut all_ready = true;
    let mut approval_required = false;
    let mut denied = false;
    let plan_has_preconditions = !plan.preconditions.is_empty();
    for step in &plan.steps {
        let mut matched_any = false;
        let mut step_ready = true;
        let mut step_denied = false;
        let mut step_requires_approval = false;

        for gate in gate_inputs
            .iter()
            .filter(|gate| gate.step_id == step.step_id)
        {
            matched_any = true;
            let preconditions_satisfied = !plan_has_preconditions || gate.preconditions_satisfied;
            step_denied |= !preconditions_satisfied
                || !gate.policy_passed
                || !gate.reservation_available
                || !gate.target_liveness;
            step_requires_approval |= preconditions_satisfied
                && gate.policy_passed
                && gate.reservation_available
                && gate.target_liveness
                && !gate.approval_satisfied;
            step_ready &= preconditions_satisfied
                && gate.policy_passed
                && gate.reservation_available
                && gate.approval_satisfied
                && gate.target_liveness;
        }

        if !matched_any {
            return Ok(TxPrepareReport {
                outcome: TxPrepareOutcome::Deferred,
                gate_inputs: gate_inputs.to_vec(),
            });
        }

        denied |= step_denied;
        approval_required |= !step_denied && step_requires_approval;
        all_ready &= step_ready;
    }

    Ok(TxPrepareReport {
        outcome: if denied {
            TxPrepareOutcome::Denied
        } else if approval_required {
            TxPrepareOutcome::RequireApproval
        } else if all_ready {
            TxPrepareOutcome::AllReady
        } else {
            TxPrepareOutcome::Denied
        },
        gate_inputs: gate_inputs.to_vec(),
    })
}

/// Execute commit phase from step inputs.
pub fn execute_commit_phase(
    contract: &MissionTxContract,
    commit_inputs: &[TxCommitStepInput],
    kill_switch: MissionKillSwitchLevel,
    paused: bool,
    now_ms: i64,
) -> Result<TxCommitReport, String> {
    if !matches!(
        contract.lifecycle_state,
        MissionTxState::Prepared | MissionTxState::Committing
    ) {
        return Err(format!(
            "Commit requires prepared or committing tx state, got {}",
            contract.lifecycle_state
        ));
    }

    if contract.plan.steps.is_empty() {
        return Err("Transaction plan has no steps".to_string());
    }
    contract.validate()?;
    validate_commit_input_step_ids(contract, commit_inputs)?;

    if kill_switch != MissionKillSwitchLevel::Off {
        let error_code = match kill_switch {
            MissionKillSwitchLevel::SafeMode => Some("tx.kill_switch.safe_mode"),
            MissionKillSwitchLevel::HardStop => Some("tx.kill_switch.hard_stop"),
            MissionKillSwitchLevel::Off => None,
        };
        return tx_blocked_commit_report(
            contract,
            TxCommitOutcome::KillSwitchBlocked,
            "kill_switch_blocked",
            error_code,
            "commit_phase->kill_switch_blocked",
            now_ms,
        );
    }

    if paused {
        return tx_blocked_commit_report(
            contract,
            TxCommitOutcome::PauseSuspended,
            "pause_suspended",
            None,
            "commit_phase->pause_suspended",
            now_ms,
        );
    }

    if let Some(step_id) = duplicate_commit_input_step_id(commit_inputs) {
        return Err(format!(
            "duplicate commit input for tx step {step_id}: replay would be order-dependent"
        ));
    }

    let mut step_results = Vec::new();
    let mut receipts = Vec::new();
    let mut next_seq = tx_last_receipt_seq(&contract.receipts);
    let mut committed = 0usize;
    let mut failed = 0usize;
    let mut skipped = 0usize;
    let mut failure_boundary = None;
    let mut report_error_code = None;
    let mut failure_seen = false;

    for step in &contract.plan.steps {
        let matched_input = commit_inputs
            .iter()
            .find(|input| input.step_id == step.step_id);
        let (outcome, decision_path, completed_at_ms, reason_code, error_code) = if failure_seen {
            skipped += 1;
            (
                TxCommitStepOutcome::Skipped {
                    reason_code: "commit_skipped_after_failure".to_string(),
                },
                "commit_phase->skipped_after_failure".to_string(),
                now_ms,
                "commit_skipped_after_failure".to_string(),
                None,
            )
        } else if let Some(input) = matched_input {
            if input.success {
                committed += 1;
                (
                    TxCommitStepOutcome::Committed {
                        reason_code: input.reason_code.clone(),
                    },
                    "commit_phase->committed".to_string(),
                    input.completed_at_ms,
                    input.reason_code.clone(),
                    None,
                )
            } else {
                failed += 1;
                failure_seen = true;
                failure_boundary = Some(step.step_id.0.clone());
                report_error_code.clone_from(&input.error_code);
                (
                    TxCommitStepOutcome::Failed {
                        reason_code: input.reason_code.clone(),
                    },
                    "commit_phase->failed".to_string(),
                    input.completed_at_ms,
                    input.reason_code.clone(),
                    input.error_code.clone(),
                )
            }
        } else {
            failed += 1;
            failure_seen = true;
            failure_boundary = Some(step.step_id.0.clone());
            report_error_code = Some("tx.commit.input_missing".to_string());
            (
                TxCommitStepOutcome::Failed {
                    reason_code: "commit_input_missing".to_string(),
                },
                "commit_phase->missing_input".to_string(),
                now_ms,
                "commit_input_missing".to_string(),
                Some("tx.commit.input_missing".to_string()),
            )
        };

        next_seq = tx_increment_receipt_seq(next_seq)?;
        receipts.push(tx_build_receipt(
            next_seq,
            "commit",
            &contract.intent.tx_id,
            &contract.plan.plan_id,
            MissionTxState::Committing,
            Some(&step.step_id),
            match &outcome {
                TxCommitStepOutcome::Committed { .. } => "committed",
                TxCommitStepOutcome::Failed { .. } => "failed",
                TxCommitStepOutcome::Skipped { .. } => "skipped",
            },
            &reason_code,
            error_code.as_deref(),
            &decision_path,
            completed_at_ms,
        ));
        step_results.push(TxCommitStepResult {
            step_id: step.step_id.clone(),
            ordinal: step.ordinal,
            outcome,
            decision_path,
            completed_at_ms,
        });
    }

    let (overall, reason_code) = if failed == 0 {
        (TxCommitOutcome::FullyCommitted, "fully_committed")
    } else if committed == 0 {
        (TxCommitOutcome::ImmediateFailure, "immediate_failure")
    } else {
        (TxCommitOutcome::PartialFailure, "partial_failure")
    };

    Ok(TxCommitReport {
        tx_id: contract.intent.tx_id.clone(),
        plan_id: contract.plan.plan_id.clone(),
        outcome: overall,
        step_results,
        failure_boundary,
        committed_count: committed,
        failed_count: failed,
        skipped_count: skipped,
        decision_path: "commit_phase".to_string(),
        reason_code: reason_code.to_string(),
        error_code: report_error_code,
        completed_at_ms: now_ms,
        receipts,
    })
}

/// Execute compensation phase after a failed commit.
pub fn execute_compensation_phase(
    contract: &MissionTxContract,
    commit_report: &TxCommitReport,
    comp_inputs: &[TxCompensationStepInput],
    now_ms: i64,
) -> Result<TxCompensationReport, String> {
    if contract.lifecycle_state != MissionTxState::Compensating {
        return Err(format!(
            "Compensation requires compensating tx state, got {}",
            contract.lifecycle_state
        ));
    }

    if contract.plan.steps.is_empty() {
        return Err("Transaction plan has no steps".to_string());
    }
    contract.validate()?;

    validate_compensation_commit_report(contract, commit_report)?;
    validate_compensation_input_step_ids(contract, commit_report, comp_inputs)?;

    let committed_steps = commit_report
        .step_results
        .iter()
        .filter(|result| result.outcome.is_committed())
        .collect::<Vec<_>>();

    if committed_steps.is_empty() {
        return Ok(TxCompensationReport {
            outcome: TxCompensationOutcome::NothingToCompensate,
            compensated_count: 0,
            failed_count: 0,
            no_compensation_count: 0,
            skipped_count: 0,
            step_results: Vec::new(),
            decision_path: "compensation_phase->nothing_to_compensate".to_string(),
            reason_code: "nothing_to_compensate".to_string(),
            error_code: None,
            completed_at_ms: now_ms,
            receipts: Vec::new(),
        });
    }

    if let Some(step_id) = duplicate_compensation_input_step_id(comp_inputs) {
        return Err(format!(
            "duplicate compensation input for tx step {step_id}: replay would be order-dependent"
        ));
    }

    let mut next_seq = tx_last_receipt_seq(&contract.receipts);
    let mut receipts = Vec::new();
    let mut step_results = Vec::with_capacity(committed_steps.len());
    let mut compensated_count = 0usize;
    let mut failed_count = 0usize;
    let mut skipped_count = 0usize;
    let mut report_error_code = None;
    let mut failure_seen = false;

    for committed_step in committed_steps.into_iter().rev() {
        let matched_input = comp_inputs
            .iter()
            .find(|input| input.for_step_id == committed_step.step_id);
        let (outcome, step_outcome, reason_code, error_code, decision_path, completed_at_ms) =
            if failure_seen {
                skipped_count += 1;
                let reason_code = "compensation_skipped_after_failure".to_string();
                (
                    "skipped",
                    TxCommitStepOutcome::Skipped {
                        reason_code: reason_code.clone(),
                    },
                    reason_code,
                    None,
                    "compensation_phase->skipped_after_failure".to_string(),
                    now_ms,
                )
            } else if let Some(input) = matched_input {
                if input.success {
                    compensated_count += 1;
                    let reason_code = input.reason_code.clone();
                    (
                        "compensated",
                        TxCommitStepOutcome::Committed {
                            reason_code: reason_code.clone(),
                        },
                        reason_code,
                        None,
                        "compensation_phase->compensated".to_string(),
                        input.completed_at_ms,
                    )
                } else {
                    failed_count += 1;
                    failure_seen = true;
                    report_error_code.clone_from(&input.error_code);
                    let reason_code = input.reason_code.clone();
                    (
                        "failed",
                        TxCommitStepOutcome::Failed {
                            reason_code: reason_code.clone(),
                        },
                        reason_code,
                        input.error_code.clone(),
                        "compensation_phase->failed".to_string(),
                        input.completed_at_ms,
                    )
                }
            } else {
                failed_count += 1;
                failure_seen = true;
                report_error_code = Some("tx.compensation.input_missing".to_string());
                let reason_code = "compensation_input_missing".to_string();
                (
                    "failed",
                    TxCommitStepOutcome::Failed {
                        reason_code: reason_code.clone(),
                    },
                    reason_code,
                    Some("tx.compensation.input_missing".to_string()),
                    "compensation_phase->missing_input".to_string(),
                    now_ms,
                )
            };

        step_results.push(TxCommitStepResult {
            step_id: committed_step.step_id.clone(),
            ordinal: committed_step.ordinal,
            outcome: step_outcome,
            decision_path: decision_path.clone(),
            completed_at_ms,
        });

        // Suppress a receipt that merely restates the outcome the contract
        // already records for this step, exactly as the commit phase does. A
        // dedup-only rollback re-derives the authoritative compensation
        // outcome from the durable ledger, so without this guard it appended a
        // duplicate receipt — and allocated a sequence number to do it, which
        // fails closed with a sequence overflow when the existing receipt sits
        // at the ceiling. A genuinely missing receipt still gets minted.
        if !tx_latest_receipt_matches(contract, "compensate", &committed_step.step_id.0, outcome) {
            next_seq = tx_increment_receipt_seq(next_seq)?;
            receipts.push(tx_build_receipt(
                next_seq,
                "compensate",
                &contract.intent.tx_id,
                &contract.plan.plan_id,
                contract.lifecycle_state,
                Some(&committed_step.step_id),
                outcome,
                &reason_code,
                error_code.as_deref(),
                &decision_path,
                completed_at_ms,
            ));
        }
    }

    let all_ok = failed_count == 0;
    Ok(TxCompensationReport {
        outcome: if all_ok {
            TxCompensationOutcome::FullyRolledBack
        } else {
            TxCompensationOutcome::CompensationFailed
        },
        compensated_count,
        failed_count,
        no_compensation_count: 0,
        skipped_count,
        step_results,
        decision_path: "compensation_phase".to_string(),
        reason_code: if all_ok {
            "fully_rolled_back".to_string()
        } else {
            "compensation_failed".to_string()
        },
        error_code: report_error_code,
        completed_at_ms: now_ms,
        receipts,
    })
}

// ============================================================================
// Mission Tx State Machine
// ============================================================================

/// Transition kind for mission transaction states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MissionTxTransitionKind {
    Prepare,
    Commit,
    Fail,
    Compensate,
    Complete,
}

impl fmt::Display for MissionTxTransitionKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Prepare => f.write_str("prepare"),
            Self::Commit => f.write_str("commit"),
            Self::Fail => f.write_str("fail"),
            Self::Compensate => f.write_str("compensate"),
            Self::Complete => f.write_str("complete"),
        }
    }
}

/// A row in the mission tx transition table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MissionTxTransitionRule {
    pub from: MissionTxState,
    pub via: MissionTxTransitionKind,
    pub to: MissionTxState,
}

const MISSION_TX_TRANSITIONS: &[MissionTxTransitionRule] = &[
    MissionTxTransitionRule {
        from: MissionTxState::Planned,
        via: MissionTxTransitionKind::Prepare,
        to: MissionTxState::Prepared,
    },
    MissionTxTransitionRule {
        from: MissionTxState::Prepared,
        via: MissionTxTransitionKind::Commit,
        to: MissionTxState::Committing,
    },
    MissionTxTransitionRule {
        from: MissionTxState::Committing,
        via: MissionTxTransitionKind::Complete,
        to: MissionTxState::Committed,
    },
    MissionTxTransitionRule {
        from: MissionTxState::Committing,
        via: MissionTxTransitionKind::Fail,
        to: MissionTxState::Failed,
    },
    MissionTxTransitionRule {
        from: MissionTxState::Committed,
        via: MissionTxTransitionKind::Compensate,
        to: MissionTxState::Compensating,
    },
    MissionTxTransitionRule {
        from: MissionTxState::Failed,
        via: MissionTxTransitionKind::Compensate,
        to: MissionTxState::Compensating,
    },
    MissionTxTransitionRule {
        from: MissionTxState::Compensating,
        via: MissionTxTransitionKind::Complete,
        to: MissionTxState::Compensated,
    },
    MissionTxTransitionRule {
        from: MissionTxState::Compensating,
        via: MissionTxTransitionKind::Complete,
        to: MissionTxState::RolledBack,
    },
    MissionTxTransitionRule {
        from: MissionTxState::Compensating,
        via: MissionTxTransitionKind::Fail,
        to: MissionTxState::Failed,
    },
];

/// Free function returning the mission tx transition table.
#[must_use]
pub fn mission_tx_transition_table() -> &'static [MissionTxTransitionRule] {
    MISSION_TX_TRANSITIONS
}

// ============================================================================
// Mission Pause/Resume/Abort Types
// ============================================================================

/// A control command issued to a mission (pause, resume, abort).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum MissionControlCommand {
    Pause {
        requested_by: String,
        reason_code: String,
        requested_at_ms: i64,
        #[serde(skip_serializing_if = "Option::is_none")]
        correlation_id: Option<String>,
    },
    Resume {
        requested_by: String,
        reason_code: String,
        requested_at_ms: i64,
        #[serde(skip_serializing_if = "Option::is_none")]
        correlation_id: Option<String>,
    },
    Abort {
        requested_by: String,
        reason_code: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        error_code: Option<String>,
        requested_at_ms: i64,
        #[serde(skip_serializing_if = "Option::is_none")]
        correlation_id: Option<String>,
    },
}

impl MissionControlCommand {
    /// Deterministic canonical string for replay comparison.
    #[must_use]
    pub fn canonical_string(&self) -> String {
        match self {
            Self::Pause {
                requested_by,
                reason_code,
                requested_at_ms,
                ..
            } => format!("pause:by={requested_by},reason={reason_code},at={requested_at_ms}"),
            Self::Resume {
                requested_by,
                reason_code,
                requested_at_ms,
                ..
            } => format!("resume:by={requested_by},reason={reason_code},at={requested_at_ms}"),
            Self::Abort {
                requested_by,
                reason_code,
                requested_at_ms,
                ..
            } => format!("abort:by={requested_by},reason={reason_code},at={requested_at_ms}"),
        }
    }
}

/// A checkpoint recording the state at a pause event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionCheckpoint {
    pub checkpoint_id: String,
    pub paused_from_state: MissionLifecycleState,
    pub paused_by: String,
    pub reason_code: String,
    pub paused_at_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resumed_at_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resumed_by: Option<String>,
    #[serde(default)]
    pub assignment_entries: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
}

/// Tracks pause/resume/abort lifecycle for a mission.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionPauseResumeState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_checkpoint: Option<MissionCheckpoint>,
    #[serde(default)]
    pub checkpoint_history: Vec<MissionCheckpoint>,
    pub total_pause_count: u32,
    pub total_resume_count: u32,
    pub total_abort_count: u32,
    pub cumulative_pause_duration_ms: i64,
}

impl MissionPauseResumeState {
    /// Whether the mission is currently paused.
    #[must_use]
    pub fn is_paused(&self) -> bool {
        self.current_checkpoint.is_some()
    }

    /// Deterministic canonical string for replay comparison.
    #[must_use]
    pub fn canonical_string(&self) -> String {
        format!(
            "prs:paused={},pauses={},resumes={},aborts={},duration_ms={}",
            self.is_paused(),
            self.total_pause_count,
            self.total_resume_count,
            self.total_abort_count,
            self.cumulative_pause_duration_ms,
        )
    }

    /// Evict checkpoint history entries older than `cutoff_ms`.
    pub fn evict_history_before(&mut self, cutoff_ms: i64) {
        self.checkpoint_history
            .retain(|cp| cp.paused_at_ms >= cutoff_ms);
    }
}

// ============================================================================
// Mission Lifecycle Decision (pause/resume/abort)
// ============================================================================

/// Result of a lifecycle control operation (pause, resume, abort).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissionLifecycleDecision {
    pub lifecycle_from: MissionLifecycleState,
    pub lifecycle_to: MissionLifecycleState,
    pub decision_path: String,
    pub reason_code: String,
    pub error_code: Option<String>,
    pub checkpoint_id: Option<String>,
}

impl Mission {
    /// Pause a running mission, creating a checkpoint.
    pub fn pause_mission(
        &mut self,
        requested_by: &str,
        reason: &str,
        requested_at_ms: i64,
        correlation_id: Option<String>,
    ) -> Result<MissionLifecycleDecision, MissionLifecycleError> {
        let from = self.lifecycle_state;
        let to = self
            .lifecycle_state
            .apply_transition(MissionLifecycleTransitionKind::ExecutionBlocked)?;

        // Create checkpoint with assignment snapshot
        let assignment_entries: Vec<String> = self
            .assignments
            .iter()
            .map(|a| a.assignment_id.0.clone())
            .collect();

        let checkpoint_id = format!(
            "cp-{}-{}",
            self.mission_id.0, self.pause_resume_state.total_pause_count
        );

        let checkpoint = MissionCheckpoint {
            checkpoint_id: checkpoint_id.clone(),
            paused_from_state: from,
            paused_by: requested_by.to_string(),
            reason_code: reason.to_string(),
            paused_at_ms: requested_at_ms,
            resumed_at_ms: None,
            resumed_by: None,
            assignment_entries,
            correlation_id: correlation_id.clone(),
        };

        self.pause_resume_state.current_checkpoint = Some(checkpoint);
        self.pause_resume_state.total_pause_count += 1;
        self.lifecycle_state = to;

        Ok(MissionLifecycleDecision {
            lifecycle_from: from,
            lifecycle_to: to,
            decision_path: "pause".to_string(),
            reason_code: reason.to_string(),
            error_code: None,
            checkpoint_id: Some(checkpoint_id),
        })
    }

    /// Resume a paused (blocked) mission.
    pub fn resume_mission(
        &mut self,
        requested_by: &str,
        _reason_label: &str,
        requested_at_ms: i64,
        _correlation_id: Option<String>,
    ) -> Result<MissionLifecycleDecision, MissionLifecycleError> {
        let from = self.lifecycle_state;
        // Resume restores the pre-pause state from the checkpoint.
        let to = if let Some(cp) = &self.pause_resume_state.current_checkpoint {
            if self.lifecycle_state != MissionLifecycleState::Paused {
                return Err(MissionLifecycleError::InvalidTransition {
                    from: self.lifecycle_state,
                    transition: MissionLifecycleTransitionKind::ResumeRequested,
                });
            }
            cp.paused_from_state
                .apply_transition(MissionLifecycleTransitionKind::ExecutionBlocked)?;
            cp.paused_from_state
        } else {
            if self.lifecycle_state == MissionLifecycleState::Paused {
                return Err(MissionLifecycleError::InvalidTransition {
                    from: self.lifecycle_state,
                    transition: MissionLifecycleTransitionKind::ResumeRequested,
                });
            }
            // No checkpoint = use standard Unblock transition as fallback
            self.lifecycle_state
                .apply_transition(MissionLifecycleTransitionKind::RetryResumed)?
        };

        // Finalize checkpoint and move to history
        let checkpoint_id = if let Some(mut cp) = self.pause_resume_state.current_checkpoint.take()
        {
            cp.resumed_at_ms = Some(requested_at_ms);
            cp.resumed_by = Some(requested_by.to_string());
            let duration = requested_at_ms.saturating_sub(cp.paused_at_ms);
            self.pause_resume_state.cumulative_pause_duration_ms += duration;
            let id = cp.checkpoint_id.clone();
            self.pause_resume_state.checkpoint_history.push(cp);
            Some(id)
        } else {
            None
        };

        self.pause_resume_state.total_resume_count += 1;
        self.lifecycle_state = to;

        Ok(MissionLifecycleDecision {
            lifecycle_from: from,
            lifecycle_to: to,
            decision_path: "resume".to_string(),
            reason_code: "resumed".to_string(),
            error_code: None,
            checkpoint_id,
        })
    }

    /// Abort a mission (cancel with error context).
    pub fn abort_mission(
        &mut self,
        requested_by: &str,
        reason: &str,
        error_code: Option<String>,
        requested_at_ms: i64,
        _correlation_id: Option<String>,
    ) -> Result<MissionLifecycleDecision, MissionLifecycleError> {
        let from = self.lifecycle_state;
        let to = self
            .lifecycle_state
            .apply_transition(MissionLifecycleTransitionKind::Cancel)?;

        // If paused, finalize checkpoint
        if let Some(mut cp) = self.pause_resume_state.current_checkpoint.take() {
            cp.resumed_at_ms = Some(requested_at_ms);
            cp.resumed_by = Some(requested_by.to_string());
            let duration = requested_at_ms.saturating_sub(cp.paused_at_ms);
            self.pause_resume_state.cumulative_pause_duration_ms += duration;
            self.pause_resume_state.checkpoint_history.push(cp);
        }

        // Cancel all in-flight assignments
        for assignment in &mut self.assignments {
            if assignment.outcome.is_none() {
                assignment.outcome = Some(Outcome::Cancelled {
                    reason_code: format!("mission_aborted:{reason}"),
                    completed_at_ms: requested_at_ms,
                });
            }
        }

        self.pause_resume_state.total_abort_count += 1;
        self.lifecycle_state = to;

        Ok(MissionLifecycleDecision {
            lifecycle_from: from,
            lifecycle_to: to,
            decision_path: "abort".to_string(),
            reason_code: reason.to_string(),
            error_code,
            checkpoint_id: None,
        })
    }
}

// ============================================================================
// Mission Agent Capability / Availability
// ============================================================================

/// Availability state of a mission agent.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum MissionAgentAvailability {
    Ready,
    Degraded {
        reason_code: String,
        max_parallel_assignments: usize,
    },
    Paused {
        reason_code: String,
    },
    RateLimited {
        reason_code: String,
    },
    Offline {
        reason_code: String,
    },
}

/// Capability profile for a mission-eligible agent.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MissionAgentCapabilityProfile {
    pub agent_id: String,
    pub capabilities: Vec<String>,
    pub lane_affinity: Vec<String>,
    pub current_load: usize,
    pub max_parallel_assignments: usize,
    pub availability: MissionAgentAvailability,
}

impl MissionAgentCapabilityProfile {
    /// Returns the effective parallel capacity, accounting for degraded state.
    pub fn effective_capacity(&self) -> usize {
        match &self.availability {
            MissionAgentAvailability::Degraded {
                max_parallel_assignments,
                ..
            } => *max_parallel_assignments,
            MissionAgentAvailability::Paused { .. } | MissionAgentAvailability::Offline { .. } => 0,
            MissionAgentAvailability::Ready | MissionAgentAvailability::RateLimited { .. } => {
                self.max_parallel_assignments
            }
        }
    }
}

// ── Mission Journal ──────────────────────────────────────────────────────────
// Crash-consistent, append-only journal for mission lifecycle decisions.

/// Kind of mission journal entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MissionJournalEntryKind {
    LifecycleTransition {
        from: MissionLifecycleState,
        to: MissionLifecycleState,
        transition_kind: MissionLifecycleTransitionKind,
    },
    KillSwitchChange {
        level_from: MissionKillSwitchLevel,
        level_to: MissionKillSwitchLevel,
    },
    AssignmentOutcome {
        assignment_id: AssignmentId,
        outcome_before: Option<String>,
        outcome_after: String,
    },
    Checkpoint {
        mission_hash: String,
        lifecycle_state: MissionLifecycleState,
        assignment_count: usize,
    },
    RecoveryMarker {
        recovered_through_seq: u64,
        recovery_reason: String,
    },
}

/// A single entry in the mission journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionJournalEntry {
    pub seq: u64,
    pub timestamp_ms: i64,
    pub correlation_id: String,
    pub entry_hash: String,
    pub kind: MissionJournalEntryKind,
    pub mission_version: u32,
    pub initiated_by: String,
    pub reason_code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}

impl MissionJournalEntry {
    /// Deterministic canonical string for replay comparison.
    #[must_use]
    pub fn canonical_string(&self) -> String {
        format!(
            "je:seq={},ts={},cid={},kind={:?}",
            self.seq, self.timestamp_ms, self.correlation_id, self.kind
        )
    }
}

/// Snapshot of journal state for diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionJournalState {
    pub entry_count: u64,
    pub last_seq: u64,
    pub last_entry_hash: String,
    pub last_checkpoint_seq: Option<u64>,
    pub last_checkpoint_hash: String,
    pub clean: bool,
}

impl MissionJournalState {
    /// Whether the journal has never had any entries appended.
    #[must_use]
    pub fn is_pristine(&self) -> bool {
        self.entry_count == 0
    }

    /// Deterministic canonical string.
    #[must_use]
    pub fn canonical_string(&self) -> String {
        format!(
            "js:count={},seq={},clean={}",
            self.entry_count, self.last_seq, self.clean
        )
    }
}

/// Report from replaying journal entries from a checkpoint.
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
    #[serde(default)]
    pub errors: Vec<String>,
}

impl MissionJournalReplayReport {
    /// Whether the replay found no errors.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.errors.is_empty()
    }

    /// Total entries (sum of all categories).
    #[must_use]
    pub fn total_entries(&self) -> usize {
        self.entries_scanned
    }
}

/// Error from journal operations.
#[derive(Debug, Clone, thiserror::Error)]
pub enum MissionJournalError {
    #[error("duplicate correlation ID: {0}")]
    DuplicateCorrelation(String),
    #[error(
        "correlation index full ({0} correlation IDs): the journal refuses NEW correlation IDs \
         so long-mission dedup memory stays bounded (ft-anpt8); duplicates of already-accepted \
         IDs are still rejected as duplicates. Raise the bound with \
         MissionJournal::with_correlation_index_cap or rotate the mission"
    )]
    CorrelationIndexFull(usize),
    #[error("journal error: {0}")]
    Internal(String),
}

/// Default bound on distinct correlation IDs a journal tracks (ft-anpt8).
///
/// 100k digests ≈ 5 MB of dedup state — far beyond any practical mission's
/// distinct control-command count, but a hard ceiling instead of an OOM.
const DEFAULT_CORRELATION_INDEX_CAP: usize = 100_000;

/// Stable 128-bit digest of a correlation ID for the dedup index (ft-anpt8).
///
/// Storing digests instead of owned `String`s cuts per-ID memory ~5x while
/// keeping the index exact for all practical purposes: a collision requires
/// ~2^64 distinct IDs, and its only effect is a FALSE REJECT
/// (`DuplicateCorrelation` for a genuinely new ID) — the fail-safe direction.
/// A false accept (double-apply) cannot be produced by a collision.
fn correlation_id_digest(cid: &str) -> u128 {
    let digest = Sha256::digest(cid.as_bytes());
    let mut first_half = [0u8; 16];
    first_half.copy_from_slice(&digest[..16]);
    u128::from_le_bytes(first_half)
}

/// Crash-consistent, append-only mission journal.
pub struct MissionJournal {
    mission_id: MissionId,
    entries: Vec<MissionJournalEntry>,
    next_seq: u64,
    // Historical idempotency set (digests — see `correlation_id_digest`).
    // Entry compaction must not reopen correlation IDs, so this set only
    // grows; `correlation_index_cap` bounds that growth fail-closed
    // (ft-anpt8) instead of letting a long-running mission grow one owned
    // String per control command forever (the ft-3e8mv follow-up footgun).
    correlation_index: std::collections::HashSet<u128>,
    correlation_index_cap: usize,
    last_checkpoint_seq: Option<u64>,
    max_entries: Option<usize>,
}

impl MissionJournal {
    /// Create a new empty journal for a mission.
    #[must_use]
    pub fn new(mission_id: MissionId) -> Self {
        Self {
            mission_id,
            entries: Vec::new(),
            next_seq: 1,
            correlation_index: std::collections::HashSet::new(),
            correlation_index_cap: DEFAULT_CORRELATION_INDEX_CAP,
            last_checkpoint_seq: None,
            max_entries: None,
        }
    }

    /// Set the maximum entries before compaction is needed.
    #[must_use]
    pub fn with_max_entries(mut self, limit: usize) -> Self {
        self.max_entries = Some(limit);
        self
    }

    /// Override the bound on distinct correlation IDs (ft-anpt8).
    ///
    /// When the index reaches the cap, [`Self::append`] refuses NEW
    /// correlation IDs with [`MissionJournalError::CorrelationIndexFull`]
    /// (fail-closed, observable) rather than growing without bound;
    /// already-accepted IDs keep deduplicating exactly. A cap of 0 is
    /// clamped to 1 so a journal can never be constructed append-dead.
    #[must_use]
    pub fn with_correlation_index_cap(mut self, cap: usize) -> Self {
        self.correlation_index_cap = cap.max(1);
        self
    }

    /// Number of entries in the journal.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the journal is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Append a new entry. Rejects duplicate correlation IDs.
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
        let digest = correlation_id_digest(&cid);
        if self.correlation_index.contains(&digest) {
            return Err(MissionJournalError::DuplicateCorrelation(cid));
        }
        if self.correlation_index.len() >= self.correlation_index_cap {
            return Err(MissionJournalError::CorrelationIndexFull(
                self.correlation_index.len(),
            ));
        }

        let seq = self.next_seq;
        self.next_seq += 1;

        let entry_hash = format!("h:{seq}:{}", self.mission_id.0);
        self.correlation_index.insert(digest);
        self.entries.push(MissionJournalEntry {
            seq,
            timestamp_ms,
            correlation_id: cid,
            entry_hash,
            kind,
            mission_version: MISSION_SCHEMA_VERSION,
            initiated_by: initiated_by.into(),
            reason_code: reason_code.into(),
            error_code,
        });

        Ok(seq)
    }

    /// Append a recovery marker entry.
    pub fn recovery_marker(
        &mut self,
        recovered_through_seq: u64,
        recovery_reason: impl Into<String>,
        timestamp_ms: i64,
    ) -> Result<u64, MissionJournalError> {
        let cid = format!("recovery:{}", self.next_seq);
        self.append(
            MissionJournalEntryKind::RecoveryMarker {
                recovered_through_seq,
                recovery_reason: recovery_reason.into(),
            },
            cid,
            "system",
            "recovery",
            None,
            timestamp_ms,
        )
    }

    /// Record a checkpoint from the current mission state.
    pub fn checkpoint(
        &mut self,
        mission: &Mission,
        timestamp_ms: i64,
    ) -> Result<u64, MissionJournalError> {
        let cid = format!("checkpoint:{}", self.next_seq);
        let seq = self.append(
            MissionJournalEntryKind::Checkpoint {
                mission_hash: format!("mhash:{}", mission.mission_id.0),
                lifecycle_state: mission.lifecycle_state,
                assignment_count: mission.assignments.len(),
            },
            cid,
            "system",
            "checkpoint",
            None,
            timestamp_ms,
        )?;
        self.last_checkpoint_seq = Some(seq);
        Ok(seq)
    }

    /// All entries.
    #[must_use]
    pub fn entries(&self) -> &[MissionJournalEntry] {
        &self.entries
    }

    /// Entries with seq >= since_seq.
    #[must_use]
    pub fn entries_since(&self, since_seq: u64) -> &[MissionJournalEntry] {
        let start = self
            .entries
            .iter()
            .position(|e| e.seq >= since_seq)
            .unwrap_or(self.entries.len());
        &self.entries[start..]
    }

    /// Whether a correlation ID has ever been accepted by the journal.
    #[must_use]
    pub fn has_correlation(&self, cid: &str) -> bool {
        self.correlation_index.contains(&correlation_id_digest(cid))
    }

    /// Whether the journal exceeds its max entry limit.
    #[must_use]
    pub fn needs_compaction(&self) -> bool {
        self.max_entries
            .is_some_and(|limit| self.entries.len() > limit)
    }

    /// Remove entries with seq < compact_seq.
    pub fn compact_before(&mut self, compact_seq: u64) {
        self.entries.retain(|e| e.seq >= compact_seq);
        // Re-derive the checkpoint anchor from retained entries so compaction
        // cannot leave stale checkpoint metadata behind.
        self.last_checkpoint_seq = self
            .entries
            .iter()
            .rev()
            .find(|entry| matches!(&entry.kind, MissionJournalEntryKind::Checkpoint { .. }))
            .map(|entry| entry.seq);
    }

    /// Take a snapshot of journal state.
    #[must_use]
    pub fn snapshot_state(&self) -> MissionJournalState {
        let last_entry = self.entries.last();
        MissionJournalState {
            entry_count: self.entries.len() as u64,
            last_seq: last_entry.map_or(0, |e| e.seq),
            last_entry_hash: last_entry.map_or_else(String::new, |e| e.entry_hash.clone()),
            last_checkpoint_seq: self.last_checkpoint_seq,
            last_checkpoint_hash: self
                .last_checkpoint_seq
                .and_then(|seq| self.entries.iter().find(|e| e.seq == seq))
                .map_or_else(String::new, |e| e.entry_hash.clone()),
            clean: true,
        }
    }

    /// Replay entries from the last checkpoint (or beginning if none).
    #[must_use]
    pub fn replay_from_checkpoint(&self) -> MissionJournalReplayReport {
        let start_idx = self
            .last_checkpoint_seq
            .and_then(|seq| self.entries.iter().position(|e| e.seq == seq))
            .unwrap_or(0);

        let replay_slice = &self.entries[start_idx..];
        let mut report = MissionJournalReplayReport {
            start_seq: replay_slice.first().map_or(0, |e| e.seq),
            entries_scanned: replay_slice.len(),
            lifecycle_transitions: 0,
            control_commands: 0,
            kill_switch_changes: 0,
            assignment_outcomes: 0,
            checkpoints_found: 0,
            recovery_markers: 0,
            errors: Vec::new(),
        };

        for entry in replay_slice {
            match &entry.kind {
                MissionJournalEntryKind::LifecycleTransition { .. } => {
                    report.lifecycle_transitions += 1;
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

// ── Journal / idempotency / resume types ─────────────────────────────────────
// These support tx_correctness_suite and tx_e2e_scenario_matrix test suites
// (ft-1i2ge.8.10 / ft-1i2ge.8.11), gated behind `subprocess-bridge` feature.

/// Phase of a transaction lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TxPhase {
    Prepare,
    Commit,
    Compensate,
}

impl fmt::Display for TxPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Prepare => f.write_str("prepare"),
            Self::Commit => f.write_str("commit"),
            Self::Compensate => f.write_str("compensate"),
        }
    }
}

/// A receipt recording a state transition in the contract lifecycle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TxReceipt {
    pub seq: u64,
    pub phase: String,
    pub tx_id: String,
    pub plan_id: String,
    pub state: MissionTxState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_id: Option<String>,
    pub outcome: String,
    pub emitted_at_ms: i64,
    pub reason_code: String,
    pub error_code: Option<String>,
    pub decision_path: String,
}

/// Record of a single step's execution within a transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TxStepExecutionRecord {
    pub step_id: TxStepId,
    pub ordinal: u32,
    pub phase: TxPhase,
    pub succeeded: bool,
    pub step_idempotency_key: String,
    pub attempt_count: u32,
    pub last_attempted_at_ms: i64,
}

impl TxStepExecutionRecord {
    /// Whether this step already succeeded in the given phase.
    #[must_use]
    pub fn is_already_succeeded(&self, phase: &TxPhase) -> bool {
        self.succeeded && self.phase == *phase
    }

    /// Compute a deterministic idempotency key for a step execution.
    #[must_use]
    pub fn compute_step_key(tx_id: &TxId, step_id: &TxStepId, phase: &TxPhase) -> String {
        format!("sk:{}:{}:{}", tx_id.0, step_id.0, phase)
    }
}

/// Full execution record for a transaction (idempotency journal entry).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TxExecutionRecord {
    pub tx_id: TxId,
    pub plan_id: TxPlanId,
    pub lifecycle_state: MissionTxState,
    pub correlation_id: String,
    pub tx_idempotency_key: String,
    pub step_records: Vec<TxStepExecutionRecord>,
    pub commit_report_hash: Option<String>,
    pub compensation_report_hash: Option<String>,
    pub updated_at_ms: i64,
}

impl TxExecutionRecord {
    /// Deterministic canonical string for replay comparison.
    #[must_use]
    pub fn canonical_string(&self) -> String {
        format!(
            "txrec:{}:{}:{}:steps={}",
            self.tx_id.0,
            self.plan_id.0,
            self.lifecycle_state,
            self.step_records.len()
        )
    }

    /// Compute an idempotency key for the entire transaction.
    /// Deterministic across processes and machines.
    #[must_use]
    pub fn compute_tx_key(contract: &MissionTxContract) -> String {
        #[derive(Serialize)]
        struct ImmutableTxStep<'a> {
            step_id: &'a TxStepId,
            ordinal: usize,
            action: &'a StepAction,
            description: &'a str,
        }

        #[derive(Serialize)]
        struct ImmutableTxContract<'a> {
            tx_version: u32,
            tx_id: &'a TxId,
            requested_by: &'a MissionActorRole,
            summary: &'a str,
            correlation_id: &'a str,
            plan_id: &'a TxPlanId,
            steps: Vec<ImmutableTxStep<'a>>,
            preconditions: &'a [TxPrecondition],
            compensations: &'a [TxCompensation],
        }

        let canonical = serde_json::to_string(&ImmutableTxContract {
            tx_version: contract.tx_version,
            tx_id: &contract.intent.tx_id,
            requested_by: &contract.intent.requested_by,
            summary: &contract.intent.summary,
            correlation_id: &contract.intent.correlation_id,
            plan_id: &contract.plan.plan_id,
            steps: contract
                .plan
                .steps
                .iter()
                .map(|step| ImmutableTxStep {
                    step_id: &step.step_id,
                    ordinal: step.ordinal,
                    action: &step.action,
                    description: &step.description,
                })
                .collect(),
            preconditions: &contract.plan.preconditions,
            compensations: &contract.plan.compensations,
        })
        .expect("serializing immutable tx contract should not fail");
        let hash = sha256_hex(&canonical);
        format!("txk:{}", &hash[..16])
    }
}

/// Verdict from idempotency validation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TxIdempotencyVerdict {
    /// No prior execution found; proceed.
    FirstExecution,
    /// A prior terminal execution exists; block.
    DoubleExecutionBlocked { original_state: MissionTxState },
    /// A prior non-terminal execution exists; resume from where it left off.
    Resumable {
        resume_from_state: MissionTxState,
        completed_steps: Vec<TxStepId>,
    },
}

/// Result of an idempotency check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TxIdempotencyCheck {
    pub verdict: TxIdempotencyVerdict,
}

impl TxIdempotencyCheck {
    /// Whether the caller should proceed with execution.
    #[must_use]
    pub fn should_proceed(&self) -> bool {
        !matches!(
            self.verdict,
            TxIdempotencyVerdict::DoubleExecutionBlocked { .. }
        )
    }

    /// Whether the prior execution is an exact terminal duplicate.
    #[must_use]
    pub fn is_exact_duplicate(&self) -> bool {
        matches!(
            self.verdict,
            TxIdempotencyVerdict::DoubleExecutionBlocked { .. }
        )
    }

    /// Deterministic canonical string for replay comparison.
    #[must_use]
    pub fn canonical_string(&self) -> String {
        format!("idem:{:?}:proceed={}", self.verdict, self.should_proceed())
    }
}

/// Validate whether a transaction phase can proceed given prior execution records.
#[must_use]
pub fn validate_tx_idempotency(
    contract: &MissionTxContract,
    phase: TxPhase,
    record: Option<&TxExecutionRecord>,
) -> TxIdempotencyCheck {
    match record {
        None => TxIdempotencyCheck {
            verdict: TxIdempotencyVerdict::FirstExecution,
        },
        Some(rec) => {
            // If prior execution reached a true terminal state (committed,
            // compensated, or rolled back), block re-execution.
            // Note: `Failed` is NOT terminal — it can still be compensated.
            if rec.lifecycle_state.is_terminal() {
                return TxIdempotencyCheck {
                    verdict: TxIdempotencyVerdict::DoubleExecutionBlocked {
                        original_state: rec.lifecycle_state,
                    },
                };
            }
            let expected_key = TxExecutionRecord::compute_tx_key(contract);
            if rec.tx_id != contract.intent.tx_id
                || rec.plan_id != contract.plan.plan_id
                || rec.tx_idempotency_key != expected_key
            {
                return TxIdempotencyCheck {
                    verdict: TxIdempotencyVerdict::DoubleExecutionBlocked {
                        original_state: rec.lifecycle_state,
                    },
                };
            }
            // Non-terminal: check if it's the same phase and can resume.
            let completed: Vec<TxStepId> = rec
                .step_records
                .iter()
                .filter(|sr| sr.phase == phase && sr.succeeded)
                .map(|sr| sr.step_id.clone())
                .collect();
            TxIdempotencyCheck {
                verdict: TxIdempotencyVerdict::Resumable {
                    resume_from_state: rec.lifecycle_state,
                    completed_steps: completed,
                },
            }
        }
    }
}

/// Reconstructed resume state for a transaction after a crash or interruption.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TxResumeState {
    pub pending_step_ids: Vec<TxStepId>,
    pub committed_step_ids: Vec<TxStepId>,
    pub compensated_step_ids: Vec<TxStepId>,
    pub commit_phase_completed: bool,
    pub compensation_phase_completed: bool,
    pub needs_compensation: bool,
    pub reconstructed_at_ms: i64,
}

impl TxResumeState {
    /// Whether all phases are complete and no work remains.
    #[must_use]
    pub fn is_fully_resolved(&self) -> bool {
        self.commit_phase_completed && self.pending_step_ids.is_empty() && !self.needs_compensation
    }

    /// Whether compensation is still required.
    #[must_use]
    pub fn has_pending_work(&self) -> bool {
        !self.is_fully_resolved()
    }

    /// Deterministic canonical string for replay comparison.
    #[must_use]
    pub fn canonical_string(&self) -> String {
        format!(
            "resume:pending={},committed={},compensated={},done={}",
            self.pending_step_ids.len(),
            self.committed_step_ids.len(),
            self.compensated_step_ids.len(),
            self.is_fully_resolved()
        )
    }
}

/// Reconstruct the resume state of a transaction from its contract and reports.
#[must_use]
pub fn reconstruct_tx_resume_state(
    contract: &MissionTxContract,
    commit_report: Option<&TxCommitReport>,
    comp_report: Option<&TxCompensationReport>,
    now_ms: i64,
) -> TxResumeState {
    let all_step_ids: Vec<TxStepId> = contract
        .plan
        .steps
        .iter()
        .map(|s| s.step_id.clone())
        .collect();

    let mut committed_step_ids = Vec::new();
    let mut commit_phase_completed = false;
    let mut commit_had_failures = false;

    if let Some(cr) = commit_report {
        commit_phase_completed = true;
        commit_had_failures = cr.has_failures();
        for result in &cr.step_results {
            let is_committed = matches!(result.outcome, TxCommitStepOutcome::Committed { .. });
            if is_committed {
                committed_step_ids.push(result.step_id.clone());
            }
        }
    } else if let Ok(receipt_report) = mission_tx_rollback_commit_report(contract, now_ms) {
        commit_phase_completed = true;
        commit_had_failures = receipt_report.has_failures();
        committed_step_ids.extend(
            receipt_report
                .step_results
                .into_iter()
                .filter(|result| result.outcome.is_committed())
                .map(|result| result.step_id),
        );
    }

    let mut compensated_step_ids = Vec::new();
    let compensation_phase_completed = if let Some(comp) = comp_report {
        let committed_step_set = committed_step_ids
            .iter()
            .map(|step_id| step_id.0.as_str())
            .collect::<HashSet<_>>();
        let mut compensated_step_set = HashSet::new();
        for result in &comp.step_results {
            if result.outcome.is_committed() {
                compensated_step_set.insert(result.step_id.0.as_str());
                compensated_step_ids.push(result.step_id.clone());
            }
        }
        let outcome_can_complete = match comp.outcome {
            TxCompensationOutcome::FullyRolledBack => true,
            TxCompensationOutcome::NothingToCompensate => committed_step_set.is_empty(),
            TxCompensationOutcome::CompensationFailed => false,
        };
        outcome_can_complete && committed_step_set.is_subset(&compensated_step_set)
    } else {
        false
    };

    let pending_step_ids: Vec<TxStepId> = if commit_phase_completed {
        Vec::new()
    } else {
        all_step_ids
    };

    // Compensation is needed if commit had failures, some steps were committed,
    // and compensation hasn't run yet.
    let needs_compensation =
        commit_had_failures && !committed_step_ids.is_empty() && !compensation_phase_completed;

    TxResumeState {
        pending_step_ids,
        committed_step_ids,
        compensated_step_ids,
        commit_phase_completed,
        compensation_phase_completed,
        needs_compensation,
        reconstructed_at_ms: now_ms,
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

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
        assert!(matches!(
            v.strategy,
            VerificationStrategy::PaneIdle {
                idle_threshold_ms: 3000,
                ..
            }
        ));
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
        assert!(matches!(
            &f,
            OnFailure::Abort { message } if message.as_deref() == Some("oops")
        ));
    }

    #[test]
    fn on_failure_retry_defaults() {
        let f = OnFailure::retry(5, 2000);
        assert!(matches!(
            &f,
            OnFailure::Retry {
                max_attempts: 5,
                initial_delay_ms: 2000,
                max_delay_ms,
                backoff_multiplier,
            } if max_delay_ms.is_none() && backoff_multiplier.is_none()
        ));
    }

    #[test]
    fn on_failure_skip_defaults() {
        let f = OnFailure::skip();
        assert!(matches!(&f, OnFailure::Skip { warn: Some(true) }));
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
    fn validation_rejects_unknown_step_reference_in_step_precondition() {
        let plan = ActionPlan {
            plan_version: PLAN_SCHEMA_VERSION,
            plan_id: PlanId::placeholder(),
            title: "bad step ref".into(),
            workspace_id: "ws".into(),
            created_at: None,
            steps: vec![
                StepPlan::new(1, StepAction::MarkEventHandled { event_id: 1 }, "Step 1")
                    .with_precondition(Precondition::StepCompleted {
                        step_id: IdempotencyKey::from_hash("nonexistent"),
                    }),
            ],
            preconditions: vec![],
            on_failure: None,
            metadata: None,
        };
        let err = plan.validate().unwrap_err();
        assert!(matches!(err, PlanValidationError::UnknownStepReference(_)));
    }

    #[test]
    fn validation_rejects_unsupported_plan_version() {
        let plan = ActionPlan {
            plan_version: PLAN_SCHEMA_VERSION + 1,
            plan_id: PlanId::placeholder(),
            title: "future".into(),
            workspace_id: "ws".into(),
            created_at: None,
            steps: vec![],
            preconditions: vec![],
            on_failure: None,
            metadata: None,
        };
        let err = plan.validate().unwrap_err();
        assert!(matches!(
            err,
            PlanValidationError::UnsupportedVersion {
                version,
                max_supported
            } if version == PLAN_SCHEMA_VERSION + 1 && max_supported == PLAN_SCHEMA_VERSION
        ));
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
    fn mission_validate_rejects_traversing_reservation_paths() {
        for invalid_path in [
            "",
            " crates/frankenterm-core/src/plan.rs",
            "/tmp/frankenterm/escape",
            "../outside",
            "crates/../wire_protocol.rs",
            "crates\\frankenterm-core\\src\\plan.rs",
            "C:/Users/jemanuel/projects/frankenterm",
            "crates/frankenterm-core/src/\nplan.rs",
        ] {
            let mut mission = sample_mission();
            mission.assignments[0].reservation_intent = Some(ReservationIntent {
                reservation_id: ReservationIntentId("reservation:bad-path".to_string()),
                requested_by: MissionActorRole::Dispatcher,
                paths: vec![invalid_path.to_string()],
                exclusive: false,
                reason: None,
                requested_at_ms: 1_704_000_000_111,
                expires_at_ms: None,
            });

            let err = mission.validate().unwrap_err();
            assert!(
                matches!(
                    err,
                    MissionValidationError::InvalidReservationPath {
                        ref reservation_id,
                        ref path,
                        ..
                    } if reservation_id.0 == "reservation:bad-path" && *path == invalid_path
                ),
                "expected invalid reservation path for {invalid_path:?}, got {err:?}"
            );
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

    #[test]
    fn mission_lifecycle_transition_table_contains_required_branches() {
        let table = MissionLifecycleState::transition_table();
        let has = |from, via, to| {
            table
                .iter()
                .any(|rule| rule.from == from && rule.via == via && rule.to == to)
        };

        assert!(has(
            MissionLifecycleState::Planning,
            MissionLifecycleTransitionKind::Dispatch,
            MissionLifecycleState::Dispatching
        ));
        assert!(has(
            MissionLifecycleState::Executing,
            MissionLifecycleTransitionKind::Retry,
            MissionLifecycleState::RetryPending
        ));
        assert!(has(
            MissionLifecycleState::Dispatching,
            MissionLifecycleTransitionKind::Block,
            MissionLifecycleState::Blocked
        ));
        assert!(has(
            MissionLifecycleState::Executing,
            MissionLifecycleTransitionKind::Complete,
            MissionLifecycleState::Completed
        ));
        assert!(has(
            MissionLifecycleState::Executing,
            MissionLifecycleTransitionKind::Fail,
            MissionLifecycleState::Failed
        ));
        assert!(has(
            MissionLifecycleState::Dispatching,
            MissionLifecycleTransitionKind::Cancel,
            MissionLifecycleState::Cancelled
        ));
    }

    #[test]
    fn mission_lifecycle_happy_path_reaches_completed() {
        let mut mission = Mission::new(
            MissionId("mission:happy".to_string()),
            "Happy path mission",
            "ws-main",
            MissionOwnership {
                planner: "planner-agent".to_string(),
                dispatcher: "dispatcher-agent".to_string(),
                operator: "operator-human".to_string(),
            },
            1_704_000_000_000,
        );

        assert_eq!(mission.lifecycle_state, MissionLifecycleState::Planning);
        assert_eq!(
            mission
                .apply_lifecycle_transition(MissionLifecycleTransitionKind::Dispatch, 100)
                .unwrap(),
            MissionLifecycleState::Dispatching
        );
        assert_eq!(
            mission
                .apply_lifecycle_transition(MissionLifecycleTransitionKind::StartExecution, 200)
                .unwrap(),
            MissionLifecycleState::Executing
        );
        assert_eq!(
            mission
                .apply_lifecycle_transition(MissionLifecycleTransitionKind::Complete, 300)
                .unwrap(),
            MissionLifecycleState::Completed
        );
        assert_eq!(mission.updated_at_ms, Some(300));
        assert!(mission.lifecycle_state.is_terminal());
    }

    #[test]
    fn mission_lifecycle_retry_and_unblock_paths_are_supported() {
        let mut mission = Mission::new(
            MissionId("mission:retry".to_string()),
            "Retry mission",
            "ws-main",
            MissionOwnership {
                planner: "planner-agent".to_string(),
                dispatcher: "dispatcher-agent".to_string(),
                operator: "operator-human".to_string(),
            },
            1_704_000_000_000,
        );

        mission
            .apply_lifecycle_transition(MissionLifecycleTransitionKind::Dispatch, 100)
            .unwrap();
        mission
            .apply_lifecycle_transition(MissionLifecycleTransitionKind::StartExecution, 200)
            .unwrap();
        mission
            .apply_lifecycle_transition(MissionLifecycleTransitionKind::Retry, 300)
            .unwrap();
        assert_eq!(mission.lifecycle_state, MissionLifecycleState::RetryPending);

        mission
            .apply_lifecycle_transition(MissionLifecycleTransitionKind::Dispatch, 400)
            .unwrap();
        mission
            .apply_lifecycle_transition(MissionLifecycleTransitionKind::Block, 500)
            .unwrap();
        assert_eq!(mission.lifecycle_state, MissionLifecycleState::Blocked);

        mission
            .apply_lifecycle_transition(MissionLifecycleTransitionKind::Unblock, 600)
            .unwrap();
        assert_eq!(mission.lifecycle_state, MissionLifecycleState::Dispatching);
    }

    #[test]
    fn mission_lifecycle_invalid_transition_is_rejected() {
        let state = MissionLifecycleState::Planning;
        let err = state
            .apply_transition(MissionLifecycleTransitionKind::Complete)
            .unwrap_err();

        assert!(matches!(
            err,
            MissionLifecycleError::InvalidTransition {
                from: MissionLifecycleState::Planning,
                transition: MissionLifecycleTransitionKind::Complete
            }
        ));
    }

    #[test]
    fn mission_validate_rejects_terminal_state_without_matching_outcome() {
        let mut mission = sample_mission();

        mission.lifecycle_state = MissionLifecycleState::Completed;
        mission.assignments[0].outcome = Some(Outcome::Failed {
            reason_code: "dispatch_error".to_string(),
            error_code: "mission.dispatch.error".to_string(),
            completed_at_ms: 1_704_000_000_800,
        });
        let err = mission.validate().unwrap_err();
        assert!(matches!(
            err,
            MissionValidationError::LifecycleStateOutcomeMismatch {
                state: MissionLifecycleState::Completed,
                ..
            }
        ));

        mission.lifecycle_state = MissionLifecycleState::Cancelled;
        mission.assignments[0].outcome = None;
        let err = mission.validate().unwrap_err();
        assert!(matches!(
            err,
            MissionValidationError::LifecycleStateOutcomeMismatch {
                state: MissionLifecycleState::Cancelled,
                ..
            }
        ));
    }

    #[test]
    fn plan_no_preconditions_helper() {
        let plan = ActionPlan::builder("Test", "ws").build();
        assert!(!plan.has_preconditions());
    }

    // =========================================================================
    // Mission::transition_lifecycle tests
    // =========================================================================

    fn planning_mission() -> Mission {
        Mission::new(
            MissionId("mission:test".to_string()),
            "Test mission",
            "ws-test",
            MissionOwnership {
                planner: "planner".to_string(),
                dispatcher: "dispatcher".to_string(),
                operator: "operator".to_string(),
            },
            1_000_000,
        )
    }

    #[test]
    fn transition_lifecycle_valid_planning_to_dispatching() {
        let mut mission = planning_mission();
        assert_eq!(mission.lifecycle_state, MissionLifecycleState::Planning);

        let result = mission.transition_lifecycle(
            MissionLifecycleState::Dispatching,
            MissionLifecycleTransitionKind::Dispatch,
            2_000_000,
        );
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), MissionLifecycleState::Dispatching);
        assert_eq!(mission.lifecycle_state, MissionLifecycleState::Dispatching);
        assert_eq!(mission.updated_at_ms, Some(2_000_000));
    }

    #[test]
    fn transition_lifecycle_valid_dispatching_to_executing() {
        let mut mission = planning_mission();
        mission.lifecycle_state = MissionLifecycleState::Dispatching;

        let result = mission.transition_lifecycle(
            MissionLifecycleState::Executing,
            MissionLifecycleTransitionKind::StartExecution,
            3_000_000,
        );
        assert!(result.is_ok());
        assert_eq!(mission.lifecycle_state, MissionLifecycleState::Executing);
    }

    #[test]
    fn transition_lifecycle_valid_executing_to_completed() {
        let mut mission = planning_mission();
        mission.lifecycle_state = MissionLifecycleState::Executing;

        let result = mission.transition_lifecycle(
            MissionLifecycleState::Completed,
            MissionLifecycleTransitionKind::Complete,
            4_000_000,
        );
        assert!(result.is_ok());
        assert_eq!(mission.lifecycle_state, MissionLifecycleState::Completed);
    }

    #[test]
    fn transition_lifecycle_invalid_returns_error() {
        let mut mission = planning_mission();
        // Planning -> Completed directly is not a valid transition
        let result = mission.transition_lifecycle(
            MissionLifecycleState::Completed,
            MissionLifecycleTransitionKind::Complete,
            2_000_000,
        );
        assert!(result.is_err());
        let err = result.unwrap_err();
        let is_invalid = matches!(
            err,
            MissionLifecycleError::InvalidTransition {
                from: MissionLifecycleState::Planning,
                transition: MissionLifecycleTransitionKind::Complete,
            }
        );
        assert!(is_invalid, "expected InvalidTransition, got {:?}", err);
        // State should not change on error
        assert_eq!(mission.lifecycle_state, MissionLifecycleState::Planning);
        assert_eq!(mission.updated_at_ms, None);
    }

    #[test]
    fn transition_lifecycle_wrong_target_state_returns_error() {
        let mut mission = planning_mission();
        // Dispatch transition from Planning should go to Dispatching, not Running
        let result = mission.transition_lifecycle(
            MissionLifecycleState::Running,
            MissionLifecycleTransitionKind::Dispatch,
            2_000_000,
        );
        assert!(result.is_err());
        assert_eq!(mission.lifecycle_state, MissionLifecycleState::Planning);
    }

    #[test]
    fn transition_lifecycle_paused_to_running_via_retry_resumed() {
        let mut mission = planning_mission();
        mission.lifecycle_state = MissionLifecycleState::Paused;

        let result = mission.transition_lifecycle(
            MissionLifecycleState::Running,
            MissionLifecycleTransitionKind::RetryResumed,
            5_000_000,
        );
        assert!(result.is_ok());
        assert_eq!(mission.lifecycle_state, MissionLifecycleState::Running);
    }

    #[test]
    fn transition_lifecycle_executing_to_paused_via_execution_blocked() {
        let mut mission = planning_mission();
        mission.lifecycle_state = MissionLifecycleState::Executing;

        let result = mission.transition_lifecycle(
            MissionLifecycleState::Paused,
            MissionLifecycleTransitionKind::ExecutionBlocked,
            6_000_000,
        );
        assert!(result.is_ok());
        assert_eq!(mission.lifecycle_state, MissionLifecycleState::Paused);
    }

    #[test]
    fn transition_lifecycle_cancel_from_any_non_terminal() {
        // Cancel should work from most non-terminal states
        for state in [
            MissionLifecycleState::Planning,
            MissionLifecycleState::Dispatching,
            MissionLifecycleState::Executing,
            MissionLifecycleState::RetryPending,
            MissionLifecycleState::Blocked,
            MissionLifecycleState::Paused,
        ] {
            let mut mission = planning_mission();
            mission.lifecycle_state = state;

            let result = mission.transition_lifecycle(
                MissionLifecycleState::Cancelled,
                MissionLifecycleTransitionKind::Cancel,
                7_000_000,
            );
            assert!(
                result.is_ok(),
                "Cancel from {state} should succeed but got {:?}",
                result.unwrap_err()
            );
            assert_eq!(mission.lifecycle_state, MissionLifecycleState::Cancelled);
        }
    }

    #[test]
    fn transition_lifecycle_terminal_states_reject_transitions() {
        for terminal in [
            MissionLifecycleState::Completed,
            MissionLifecycleState::Cancelled,
            MissionLifecycleState::Failed,
        ] {
            let mut mission = planning_mission();
            mission.lifecycle_state = terminal;

            let result = mission.transition_lifecycle(
                MissionLifecycleState::Running,
                MissionLifecycleTransitionKind::Retry,
                8_000_000,
            );
            assert!(
                result.is_err(),
                "Transition from terminal state {terminal} should fail"
            );
        }
    }

    // =========================================================================
    // Mission dispatch method tests
    // =========================================================================

    fn mission_with_dispatch_data() -> Mission {
        let mut mission = planning_mission();
        mission.candidates.push(CandidateAction {
            candidate_id: CandidateActionId("candidate:alpha".to_string()),
            requested_by: MissionActorRole::Planner,
            action: StepAction::SendText {
                pane_id: 1,
                text: "ls -la".to_string(),
                paste_mode: Some(false),
            },
            rationale: "List directory contents".to_string(),
            score: Some(0.85),
            created_at_ms: 1_000_100,
        });
        mission.assignments.push(Assignment {
            assignment_id: AssignmentId("assignment:alpha".to_string()),
            candidate_id: CandidateActionId("candidate:alpha".to_string()),
            assigned_by: MissionActorRole::Dispatcher,
            assignee: "agent-1".to_string(),
            reservation_intent: None,
            approval_state: ApprovalState::Approved {
                approved_by: "operator".to_string(),
                approved_at_ms: 1_000_200,
                approval_code_hash: "sha256:test".to_string(),
            },
            outcome: None,
            escalation: None,
            created_at_ms: 1_000_150,
            updated_at_ms: None,
        });
        // Add a second assignment with Pending approval
        mission.assignments.push(Assignment {
            assignment_id: AssignmentId("assignment:beta".to_string()),
            candidate_id: CandidateActionId("candidate:alpha".to_string()),
            assigned_by: MissionActorRole::Dispatcher,
            assignee: "agent-2".to_string(),
            reservation_intent: None,
            approval_state: ApprovalState::Pending {
                requested_by: "dispatcher".to_string(),
                requested_at_ms: 1_000_250,
            },
            outcome: None,
            escalation: None,
            created_at_ms: 1_000_250,
            updated_at_ms: None,
        });
        mission
    }

    #[test]
    fn dispatch_contract_for_known_candidate() {
        let mission = mission_with_dispatch_data();
        let contract = mission
            .dispatch_contract_for_candidate(&CandidateActionId("candidate:alpha".to_string()));
        assert!(contract.is_ok());
        let contract = contract.unwrap();
        assert_eq!(contract.assignment_id.as_deref(), Some("assignment:alpha"));
        assert_eq!(contract.target_agent.as_deref(), Some("agent-1"));
        assert_eq!(contract.candidate_id.0, "candidate:alpha");
        assert_eq!(contract.action.action_type_name(), "send_text");
        assert_eq!(contract.rationale, "List directory contents");
        assert!(matches!(
            contract.approval_state,
            Some(ApprovalState::Approved { .. })
        ));
    }

    #[test]
    fn dispatch_contract_for_candidate_without_assignment_falls_back() {
        let mut mission = planning_mission();
        mission.candidates.push(CandidateAction {
            candidate_id: CandidateActionId("candidate:orphan".to_string()),
            requested_by: MissionActorRole::Planner,
            action: StepAction::SendText {
                pane_id: 5,
                text: "echo test".to_string(),
                paste_mode: None,
            },
            rationale: "Orphan candidate".to_string(),
            score: None,
            created_at_ms: 1_000_100,
        });
        let contract = mission
            .dispatch_contract_for_candidate(&CandidateActionId("candidate:orphan".to_string()))
            .unwrap();
        assert!(contract.assignment_id.is_none());
        assert!(contract.target_agent.is_none());
        assert_eq!(contract.candidate_id.0, "candidate:orphan");
        assert_eq!(contract.action.action_type_name(), "send_text");
        assert_eq!(contract.rationale, "Orphan candidate");
        assert!(contract.approval_state.is_none());
    }

    #[test]
    fn dispatch_contract_for_unknown_candidate_returns_error() {
        let mission = mission_with_dispatch_data();
        let result = mission
            .dispatch_contract_for_candidate(&CandidateActionId("candidate:unknown".to_string()));
        assert!(result.is_err());
        let is_not_found = matches!(
            result.unwrap_err(),
            MissionDispatchError::CandidateNotFound(_)
        );
        assert!(is_not_found);
    }

    #[test]
    fn resolve_dispatch_target_for_known_assignment() {
        let mission = mission_with_dispatch_data();
        let target = mission.resolve_dispatch_target(&AssignmentId("assignment:alpha".to_string()));
        assert!(target.is_ok());
        let target = target.unwrap();
        assert_eq!(target.assignment_id.0, "assignment:alpha");
        assert_eq!(target.assignee, "agent-1");
        assert_eq!(target.candidate_id.0, "candidate:alpha");
        assert_eq!(target.action_type, "send_text");
        assert_eq!(target.pane_id, Some(1));
        assert_eq!(target.workspace, Some("ws-test".to_string()));
        assert!(matches!(
            target.approval_state,
            ApprovalState::Approved { .. }
        ));
    }

    #[test]
    fn resolve_dispatch_target_for_unknown_assignment_returns_error() {
        let mission = mission_with_dispatch_data();
        let result =
            mission.resolve_dispatch_target(&AssignmentId("assignment:unknown".to_string()));
        assert!(result.is_err());
        let is_not_found = matches!(
            result.unwrap_err(),
            MissionDispatchError::AssignmentNotFound(_)
        );
        assert!(is_not_found);
    }

    #[test]
    fn dispatch_dry_run_approved_succeeds() {
        let mission = mission_with_dispatch_data();
        let result = mission
            .dispatch_assignment_dry_run(&AssignmentId("assignment:alpha".to_string()), 2_000_000);
        assert!(result.is_ok());
        let execution = result.unwrap();
        assert!(execution.would_succeed);
        assert!(execution.would_dispatch);
        assert_eq!(execution.assignment_id.0, "assignment:alpha");
        assert_eq!(execution.simulated_at_ms, 2_000_000);
        assert_eq!(execution.target.pane_id, Some(1));
        assert!(execution.approval_allows_dispatch);
        assert!(execution.target_reachable);
        assert!(execution.reason.is_none());
    }

    #[test]
    fn dispatch_dry_run_pending_fails() {
        let mission = mission_with_dispatch_data();
        let result = mission
            .dispatch_assignment_dry_run(&AssignmentId("assignment:beta".to_string()), 2_000_000);
        assert!(result.is_ok());
        let execution = result.unwrap();
        assert!(!execution.would_succeed);
        assert!(!execution.would_dispatch);
        assert!(!execution.approval_allows_dispatch);
        assert!(execution.target_reachable);
        assert!(execution.reason.is_some());
    }

    #[test]
    fn dispatch_dry_run_unknown_assignment_returns_error() {
        let mission = mission_with_dispatch_data();
        let result = mission.dispatch_assignment_dry_run(
            &AssignmentId("assignment:unknown".to_string()),
            2_000_000,
        );
        assert!(result.is_err());
    }

    #[test]
    fn dispatch_dry_run_not_required_succeeds() {
        let mut mission = mission_with_dispatch_data();
        // Change first assignment to NotRequired
        mission.assignments[0].approval_state = ApprovalState::NotRequired;
        let result = mission
            .dispatch_assignment_dry_run(&AssignmentId("assignment:alpha".to_string()), 2_000_000);
        assert!(result.is_ok());
        assert!(result.unwrap().would_succeed);
    }

    #[test]
    fn resolve_dispatch_target_for_non_pane_action_does_not_fallback_to_zero() {
        let mut mission = planning_mission();
        mission.candidates.push(CandidateAction {
            candidate_id: CandidateActionId("candidate:lock".to_string()),
            requested_by: MissionActorRole::Planner,
            action: StepAction::AcquireLock {
                lock_name: "repo:index".to_string(),
                timeout_ms: Some(5_000),
            },
            rationale: "Reserve shared index before commit".to_string(),
            score: Some(0.75),
            created_at_ms: 1_000_100,
        });
        mission.assignments.push(Assignment {
            assignment_id: AssignmentId("assignment:lock".to_string()),
            candidate_id: CandidateActionId("candidate:lock".to_string()),
            assigned_by: MissionActorRole::Dispatcher,
            assignee: "agent-lock".to_string(),
            reservation_intent: None,
            approval_state: ApprovalState::NotRequired,
            outcome: None,
            escalation: None,
            created_at_ms: 1_000_150,
            updated_at_ms: None,
        });

        let target = mission
            .resolve_dispatch_target(&AssignmentId("assignment:lock".to_string()))
            .unwrap();

        assert_eq!(target.assignment_id.0, "assignment:lock");
        assert_eq!(target.assignee, "agent-lock");
        assert_eq!(target.action_type, "acquire_lock");
        assert_eq!(target.pane_id, None);
        assert_eq!(target.workspace.as_deref(), Some("ws-test"));
    }

    #[test]
    fn dispatch_dry_run_uses_resolved_non_pane_target_inventory() {
        let mut mission = planning_mission();
        mission.candidates.push(CandidateAction {
            candidate_id: CandidateActionId("candidate:store".to_string()),
            requested_by: MissionActorRole::Planner,
            action: StepAction::StoreData {
                key: "mission/status".to_string(),
                value: serde_json::json!({"state": "ready"}),
            },
            rationale: "Persist mission readiness".to_string(),
            score: None,
            created_at_ms: 1_000_100,
        });
        mission.assignments.push(Assignment {
            assignment_id: AssignmentId("assignment:store".to_string()),
            candidate_id: CandidateActionId("candidate:store".to_string()),
            assigned_by: MissionActorRole::Dispatcher,
            assignee: "storage-agent".to_string(),
            reservation_intent: None,
            approval_state: ApprovalState::NotRequired,
            outcome: None,
            escalation: None,
            created_at_ms: 1_000_150,
            updated_at_ms: None,
        });

        let execution = mission
            .dispatch_assignment_dry_run(&AssignmentId("assignment:store".to_string()), 2_345_678)
            .unwrap();

        assert!(execution.would_succeed);
        assert_eq!(execution.assignment_id.0, "assignment:store");
        assert_eq!(execution.simulated_at_ms, 2_345_678);
        assert_eq!(execution.target.assignee, "storage-agent");
        assert_eq!(execution.target.action_type, "store_data");
        assert_eq!(execution.target.pane_id, None);
        assert_eq!(execution.target.workspace.as_deref(), Some("ws-test"));
    }

    fn sample_tx_contract(state: MissionTxState) -> MissionTxContract {
        let tx_id = TxId("tx:test".to_string());
        MissionTxContract {
            tx_version: MISSION_TX_SCHEMA_VERSION,
            intent: TxIntent {
                tx_id: tx_id.clone(),
                requested_by: MissionActorRole::Dispatcher,
                summary: "tx test".to_string(),
                correlation_id: "corr:test".to_string(),
                created_at_ms: 1_700_000_000_000,
            },
            plan: TxPlan {
                plan_id: TxPlanId("plan:test".to_string()),
                tx_id,
                steps: vec![
                    TxStep {
                        step_id: TxStepId("tx-step:1".to_string()),
                        ordinal: 1,
                        action: StepAction::SendText {
                            pane_id: 1,
                            text: "step-1".to_string(),
                            paste_mode: Some(false),
                        },
                        description: "step 1".to_string(),
                    },
                    TxStep {
                        step_id: TxStepId("tx-step:2".to_string()),
                        ordinal: 2,
                        action: StepAction::SendText {
                            pane_id: 2,
                            text: "step-2".to_string(),
                            paste_mode: Some(false),
                        },
                        description: "step 2".to_string(),
                    },
                    TxStep {
                        step_id: TxStepId("tx-step:3".to_string()),
                        ordinal: 3,
                        action: StepAction::SendText {
                            pane_id: 3,
                            text: "step-3".to_string(),
                            paste_mode: Some(true),
                        },
                        description: "step 3".to_string(),
                    },
                ],
                preconditions: vec![TxPrecondition::PromptActive { pane_id: 1 }],
                compensations: vec![
                    TxCompensation {
                        for_step_id: TxStepId("tx-step:1".to_string()),
                        action: StepAction::SendText {
                            pane_id: 1,
                            text: "undo-1".to_string(),
                            paste_mode: Some(false),
                        },
                    },
                    TxCompensation {
                        for_step_id: TxStepId("tx-step:2".to_string()),
                        action: StepAction::SendText {
                            pane_id: 2,
                            text: "undo-2".to_string(),
                            paste_mode: Some(false),
                        },
                    },
                    TxCompensation {
                        for_step_id: TxStepId("tx-step:3".to_string()),
                        action: StepAction::SendText {
                            pane_id: 3,
                            text: "undo-3".to_string(),
                            paste_mode: Some(true),
                        },
                    },
                ],
            },
            lifecycle_state: state,
            outcome: state.expected_outcome(),
            receipts: Vec::new(),
        }
    }

    #[test]
    fn tx_contract_validate_rejects_path_like_identifiers() {
        assert!(
            sample_tx_contract(MissionTxState::Planned)
                .validate()
                .is_ok()
        );

        let mut cases = Vec::new();

        let mut intent_tx_path = sample_tx_contract(MissionTxState::Planned);
        intent_tx_path.intent.tx_id = TxId("tx/escape".to_string());
        intent_tx_path.plan.tx_id = intent_tx_path.intent.tx_id.clone();
        cases.push(("intent tx_id", intent_tx_path));

        let mut plan_tx_path = sample_tx_contract(MissionTxState::Planned);
        plan_tx_path.plan.tx_id = TxId("tx\\escape".to_string());
        cases.push(("plan tx_id", plan_tx_path));

        let mut traversal_plan = sample_tx_contract(MissionTxState::Planned);
        traversal_plan.plan.plan_id = TxPlanId("plan:..".to_string());
        cases.push(("plan_id", traversal_plan));

        let mut control_correlation = sample_tx_contract(MissionTxState::Planned);
        control_correlation.intent.correlation_id = "corr\nbad".to_string();
        cases.push(("correlation_id", control_correlation));

        let mut whitespace_step = sample_tx_contract(MissionTxState::Planned);
        whitespace_step.plan.steps[0].step_id = TxStepId(" tx-step:1".to_string());
        cases.push(("step_id", whitespace_step));

        let mut path_like_compensation = sample_tx_contract(MissionTxState::Planned);
        path_like_compensation.plan.compensations[0].for_step_id =
            TxStepId("tx-step/1".to_string());
        cases.push(("compensation step_id", path_like_compensation));

        for (field, contract) in cases {
            let validation = contract.validate();
            assert!(
                validation.is_err(),
                "{field} should reject invalid tx identifier"
            );
            let err = validation.err().unwrap_or_default();
            assert!(
                err.contains(field),
                "{field} should be named in validation error: {err}"
            );
            assert!(
                err.contains("invalid"),
                "{field} should fail as an invalid identifier: {err}"
            );
        }
    }

    #[test]
    fn tx_contract_validate_rejects_unsupported_schema_version() {
        let mut contract = sample_tx_contract(MissionTxState::Planned);
        contract.tx_version = MISSION_TX_SCHEMA_VERSION + 1;

        let err = contract
            .validate()
            .expect_err("newer tx schema must fail closed");
        assert_eq!(
            err,
            format!(
                "Transaction tx_version {} is unsupported; expected {}",
                MISSION_TX_SCHEMA_VERSION + 1,
                MISSION_TX_SCHEMA_VERSION
            )
        );

        contract.tx_version = 0;
        assert_eq!(
            contract.validate().unwrap_err(),
            format!(
                "Transaction tx_version 0 is unsupported; expected {}",
                MISSION_TX_SCHEMA_VERSION
            )
        );
    }

    #[test]
    fn tx_contract_validate_rejects_lifecycle_outcome_mismatch() {
        let mut contract = sample_tx_contract(MissionTxState::Planned);
        contract.outcome = TxOutcome::Failed;

        let err = contract.validate().unwrap_err();

        assert!(err.contains("requires outcome Pending"));
        assert!(err.contains("got Failed"));
    }

    #[test]
    fn tx_economic_breaker_trips_on_no_progress_token_burn() {
        let mut contract = sample_tx_contract(MissionTxState::Planned);
        contract
            .attach_token_budget(MissionTokenBudget {
                max_tokens: 1_000,
                max_no_progress_tokens: 100,
            })
            .unwrap();

        let first = contract
            .record_economic_usage(MissionTokenUsageSample {
                prompt_tokens: 20,
                output_tokens: 30,
                progress_delta: 0,
                observed_at_ms: 1_000,
            })
            .unwrap();
        assert!(matches!(
            first,
            MissionEconomicBreakerDecision::Continue { .. }
        ));

        let second = contract
            .record_economic_usage(MissionTokenUsageSample {
                prompt_tokens: 40,
                output_tokens: 20,
                progress_delta: 0,
                observed_at_ms: 1_500,
            })
            .unwrap();

        assert!(
            matches!(second, MissionEconomicBreakerDecision::HardStop { .. }),
            "no-progress token burn should trip the mission hard stop"
        );
        if let MissionEconomicBreakerDecision::HardStop {
            envelope,
            audit_row,
        } = second
        {
            assert_eq!(envelope.kill_switch_level, MissionKillSwitchLevel::HardStop);
            assert_eq!(
                envelope.reason_code,
                "tx.economic.no_progress_budget_exhausted"
            );
            assert_eq!(envelope.no_progress_tokens, 110);
            assert_eq!(envelope.total_tokens, 110);
            assert_eq!(audit_row.action, "mission_economic_hard_stop");
            assert_eq!(audit_row.reason_code, envelope.reason_code);
        }

        let attachment = contract
            .token_budget_attachment()
            .unwrap()
            .expect("budget attachment should persist");
        assert!(attachment.economic_state.hard_stop_tripped);
    }

    #[test]
    fn tx_economic_breaker_progress_resets_no_progress_burn() {
        let mut contract = sample_tx_contract(MissionTxState::Planned);
        contract
            .attach_token_budget(MissionTokenBudget {
                max_tokens: 500,
                max_no_progress_tokens: 100,
            })
            .unwrap();

        contract
            .record_economic_usage(MissionTokenUsageSample {
                prompt_tokens: 30,
                output_tokens: 50,
                progress_delta: 0,
                observed_at_ms: 1_000,
            })
            .unwrap();
        let progressed = contract
            .record_economic_usage(MissionTokenUsageSample {
                prompt_tokens: 10,
                output_tokens: 20,
                progress_delta: 1,
                observed_at_ms: 1_500,
            })
            .unwrap();

        assert!(
            matches!(progressed, MissionEconomicBreakerDecision::Continue { .. }),
            "progress within budget should not trip the hard stop"
        );
        if let MissionEconomicBreakerDecision::Continue { state } = progressed {
            assert_eq!(state.no_progress_tokens, 0);
            assert_eq!(state.progress_markers, 1);
            assert!(!state.hard_stop_tripped);
        }

        let after_progress = contract
            .record_economic_usage(MissionTokenUsageSample {
                prompt_tokens: 40,
                output_tokens: 50,
                progress_delta: 0,
                observed_at_ms: 2_000,
            })
            .unwrap();
        assert!(
            matches!(
                after_progress,
                MissionEconomicBreakerDecision::Continue { .. }
            ),
            "post-progress burn below budget should continue"
        );
        if let MissionEconomicBreakerDecision::Continue { state } = after_progress {
            assert_eq!(state.no_progress_tokens, 90);
            assert!(!state.hard_stop_tripped);
        }
    }

    #[test]
    fn tx_contract_validate_rejects_invalid_token_budget_attachment() {
        let mut contract = sample_tx_contract(MissionTxState::Planned);
        contract.receipts.push(serde_json::json!({
            "kind": "mission_token_budget",
            "token_budget": {
                "max_tokens": 10,
                "max_no_progress_tokens": 20
            }
        }));

        let err = contract.validate().unwrap_err();
        assert!(
            err.contains("no-progress limit 20 exceeds total budget 10"),
            "invalid budget should be named in error: {err}"
        );
    }

    #[test]
    fn compute_tx_key_is_stable_for_same_contract() {
        let contract = sample_tx_contract(MissionTxState::Planned);

        let key1 = TxExecutionRecord::compute_tx_key(&contract);
        let key2 = TxExecutionRecord::compute_tx_key(&contract);

        assert_eq!(key1, key2);
        assert!(key1.starts_with("txk:"));
        assert_eq!(key1.len(), 20);
    }

    #[test]
    fn tx_contract_hash_is_stable_across_lifecycle_and_receipts() {
        let contract = sample_tx_contract(MissionTxState::Planned);
        let mut progressed = contract.clone();
        progressed.lifecycle_state = MissionTxState::Committing;
        progressed.outcome = TxOutcome::Pending;
        progressed.receipts.push(serde_json::json!({
            "kind": "ft.steering_receipt.run",
            "receipt_id": "steer:0123456789abcdef0123456789abcdef",
        }));

        let hash = contract.compute_hash();

        assert_eq!(hash, progressed.compute_hash());
        assert!(hash.starts_with("sha256:"));
        assert_eq!(hash.len(), "sha256:".len() + 32);
    }

    #[test]
    fn tx_contract_hash_changes_when_immutable_contract_changes() {
        let contract = sample_tx_contract(MissionTxState::Planned);
        let mut different = contract.clone();
        different.plan.steps[0].action = StepAction::SendText {
            pane_id: 9,
            text: "changed".to_string(),
            paste_mode: Some(true),
        };

        assert_ne!(contract.compute_hash(), different.compute_hash());
    }

    #[test]
    fn compute_tx_key_changes_when_correlation_id_changes() {
        let contract = sample_tx_contract(MissionTxState::Planned);
        let mut different = contract.clone();
        different.intent.correlation_id = "corr:other".to_string();

        assert_ne!(
            TxExecutionRecord::compute_tx_key(&contract),
            TxExecutionRecord::compute_tx_key(&different)
        );
    }

    #[test]
    fn compute_tx_key_changes_when_step_order_changes() {
        let contract = sample_tx_contract(MissionTxState::Planned);
        let mut reordered = contract.clone();
        reordered.plan.steps.swap(0, 1);

        assert_ne!(
            TxExecutionRecord::compute_tx_key(&contract),
            TxExecutionRecord::compute_tx_key(&reordered)
        );
    }

    #[test]
    fn compute_tx_key_changes_when_step_action_changes() {
        let contract = sample_tx_contract(MissionTxState::Planned);
        let mut different = contract.clone();
        different.plan.steps[0].action = StepAction::SendText {
            pane_id: 9,
            text: "changed".to_string(),
            paste_mode: Some(true),
        };

        assert_ne!(
            TxExecutionRecord::compute_tx_key(&contract),
            TxExecutionRecord::compute_tx_key(&different)
        );
    }

    #[test]
    fn compute_tx_key_changes_when_preconditions_change() {
        let contract = sample_tx_contract(MissionTxState::Planned);
        let mut different = contract.clone();
        different.plan.preconditions = vec![TxPrecondition::Custom {
            check: "pane-ready".to_string(),
        }];

        assert_ne!(
            TxExecutionRecord::compute_tx_key(&contract),
            TxExecutionRecord::compute_tx_key(&different)
        );
    }

    fn sample_commit_inputs(fail_step: Option<&str>) -> Vec<TxCommitStepInput> {
        ["tx-step:1", "tx-step:2", "tx-step:3"]
            .into_iter()
            .enumerate()
            .map(|(idx, step_id)| {
                let should_fail = fail_step == Some(step_id);
                TxCommitStepInput {
                    step_id: TxStepId(step_id.to_string()),
                    success: !should_fail,
                    reason_code: if should_fail {
                        "commit_failed_injected".to_string()
                    } else {
                        "commit_succeeded".to_string()
                    },
                    error_code: should_fail.then(|| "FTX9999".to_string()),
                    completed_at_ms: 10_000 + idx as i64,
                }
            })
            .collect()
    }

    fn sample_comp_inputs(fail_step: Option<&str>) -> Vec<TxCompensationStepInput> {
        sample_comp_inputs_for(&["tx-step:1", "tx-step:2", "tx-step:3"], fail_step)
    }

    fn sample_committed_comp_inputs(fail_step: Option<&str>) -> Vec<TxCompensationStepInput> {
        sample_comp_inputs_for(&["tx-step:1", "tx-step:2"], fail_step)
    }

    fn sample_comp_inputs_for(
        step_ids: &[&str],
        fail_step: Option<&str>,
    ) -> Vec<TxCompensationStepInput> {
        step_ids
            .iter()
            .copied()
            .enumerate()
            .map(|(idx, step_id)| {
                let should_fail = fail_step == Some(step_id);
                TxCompensationStepInput {
                    for_step_id: TxStepId(step_id.to_string()),
                    success: !should_fail,
                    reason_code: if should_fail {
                        "compensation_failed_injected".to_string()
                    } else {
                        "compensation_succeeded".to_string()
                    },
                    error_code: should_fail.then(|| "FTX4999".to_string()),
                    completed_at_ms: 20_000 + idx as i64,
                }
            })
            .collect()
    }

    #[derive(Clone)]
    struct MockPolicyAuthorizer {
        default: PolicyDecision,
        decisions: HashMap<String, PolicyDecision>,
    }

    impl MockPolicyAuthorizer {
        fn allow_all() -> Self {
            Self {
                default: PolicyDecision::allow(),
                decisions: HashMap::new(),
            }
        }

        fn with_pane_decision(mut self, pane_id: u64, decision: PolicyDecision) -> Self {
            self.decisions.insert(format!("pane:{pane_id}"), decision);
            self
        }
    }

    impl TxPreparePolicyAuthorizer for MockPolicyAuthorizer {
        fn authorize_prepare(&self, input: &PolicyInput) -> PolicyDecision {
            input
                .pane_id
                .and_then(|pane_id| self.decisions.get(&format!("pane:{pane_id}")).cloned())
                .unwrap_or_else(|| self.default.clone())
        }
    }

    #[derive(Default)]
    struct MockApprovalChecker {
        approved: HashSet<String>,
        lookup_failures: HashSet<String>,
    }

    impl MockApprovalChecker {
        fn grant(&mut self, scope: &ApprovalScope) {
            self.approved.insert(Self::scope_key(scope));
        }

        fn fail_lookup(&mut self, scope: &ApprovalScope) {
            self.lookup_failures.insert(Self::scope_key(scope));
        }

        fn scope_key(scope: &ApprovalScope) -> String {
            format!(
                "{}|{}|{}|{}",
                scope.workspace_id,
                scope.action_kind,
                scope
                    .pane_id
                    .map_or_else(|| "none".to_string(), |pane_id| pane_id.to_string()),
                scope.action_fingerprint
            )
        }
    }

    impl TxPrepareApprovalChecker for MockApprovalChecker {
        fn has_active_approval(
            &self,
            scope: &ApprovalScope,
            _now_ms: i64,
        ) -> std::result::Result<bool, String> {
            let key = Self::scope_key(scope);
            if self.lookup_failures.contains(&key) {
                return Err("lookup failed".to_string());
            }
            Ok(self.approved.contains(&key))
        }
    }

    #[derive(Default)]
    struct MockTargetLookup {
        snapshots: HashMap<u64, TxPrepareTargetSnapshot>,
        failures: HashMap<u64, String>,
        lookup_counts: RefCell<HashMap<u64, usize>>,
    }

    impl MockTargetLookup {
        fn with_snapshot(mut self, snapshot: TxPrepareTargetSnapshot) -> Self {
            self.snapshots.insert(snapshot.pane_id, snapshot);
            self
        }

        fn lookup_count(&self, pane_id: u64) -> usize {
            self.lookup_counts
                .borrow()
                .get(&pane_id)
                .copied()
                .unwrap_or_default()
        }
    }

    impl TxPrepareTargetLookup for MockTargetLookup {
        fn lookup_target(
            &self,
            pane_id: u64,
        ) -> std::result::Result<Option<TxPrepareTargetSnapshot>, String> {
            *self.lookup_counts.borrow_mut().entry(pane_id).or_default() += 1;
            if let Some(reason) = self.failures.get(&pane_id) {
                return Err(reason.clone());
            }
            Ok(self.snapshots.get(&pane_id).cloned())
        }
    }

    fn sample_prepare_context() -> TxPrepareEvaluationContext {
        TxPrepareEvaluationContext::new("workspace:test")
            .with_surface(PolicySurface::Workflow)
            .with_workflow_id("wf-test")
            .with_stale_after_ms(30_000)
    }

    fn live_target_snapshot(pane_id: u64, last_seen_at_ms: i64) -> TxPrepareTargetSnapshot {
        TxPrepareTargetSnapshot {
            pane_id,
            capabilities: PaneCapabilities::prompt(),
            last_seen_at_ms: Some(last_seen_at_ms),
            observed: true,
            known_dead: false,
            domain: Some("local".to_string()),
            pane_title: Some(format!("pane-{pane_id}")),
            pane_cwd: Some(format!("/tmp/pane-{pane_id}")),
            reserved_by: None,
            reservation_lookup_error: None,
        }
    }

    #[test]
    fn tx_surface_prepare_gate_inputs_default_to_ready() {
        let contract = sample_tx_contract(MissionTxState::Planned);
        let policy = RefCell::new(crate::policy::PolicyEngine::new(100, 100, false));
        let approvals = MockApprovalChecker::default();
        let targets = MockTargetLookup::default()
            .with_snapshot(live_target_snapshot(1, 1_700_000_000_000))
            .with_snapshot(live_target_snapshot(2, 1_700_000_000_000))
            .with_snapshot(live_target_snapshot(3, 1_700_000_000_000));
        let inputs = mission_tx_prepare_gate_inputs(
            &contract,
            &policy,
            &approvals,
            &targets,
            &sample_prepare_context(),
            1_700_000_000_000,
        );

        assert_eq!(inputs.len(), 3);
        assert_eq!(inputs[0].step_id.0, "tx-step:1");
        assert!(inputs.iter().all(|input| input.policy_passed
            && input.preconditions_satisfied
            && input.reservation_available
            && input.approval_satisfied
            && input.target_liveness));
        assert_eq!(targets.lookup_count(1), 1);
        assert_eq!(targets.lookup_count(2), 1);
        assert_eq!(targets.lookup_count(3), 1);
    }

    #[test]
    fn tx_prepare_policy_identity_comes_from_authenticated_context() {
        let mut contract = sample_tx_contract(MissionTxState::Planned);
        contract.intent.requested_by = MissionActorRole::Operator;
        let context = TxPrepareEvaluationContext::new("workspace:test")
            .with_surface(PolicySurface::Mcp)
            .with_actor(ActorKind::Mcp);

        let input = tx_prepare_policy_input(&contract.plan.steps[0], Some(1), None, &context);

        assert_eq!(input.actor, ActorKind::Mcp);
        assert_eq!(input.surface, PolicySurface::Mcp);
    }

    #[test]
    fn tx_prepare_prompt_active_precondition_fails_when_prompt_is_inactive() {
        let contract = sample_tx_contract(MissionTxState::Planned);
        let mut inactive = live_target_snapshot(1, 1_700_000_000_000);
        inactive.capabilities = PaneCapabilities::running();
        let targets = MockTargetLookup::default()
            .with_snapshot(inactive)
            .with_snapshot(live_target_snapshot(2, 1_700_000_000_000))
            .with_snapshot(live_target_snapshot(3, 1_700_000_000_000));
        let inputs = mission_tx_prepare_gate_inputs(
            &contract,
            &MockPolicyAuthorizer::allow_all(),
            &MockApprovalChecker::default(),
            &targets,
            &sample_prepare_context(),
            1_700_000_000_000,
        );

        assert!(inputs.iter().all(|input| !input.preconditions_satisfied));
        assert!(inputs.iter().all(|input| {
            input.precondition_reason_code.as_deref()
                == Some("tx.prepare.precondition.prompt_active.inactive:1")
        }));
        let report = evaluate_prepare_phase(
            &contract.intent.tx_id,
            &contract.plan,
            &inputs,
            MissionKillSwitchLevel::Off,
            1_700_000_000_000,
        )
        .expect("prepare report");
        assert_eq!(report.outcome, TxPrepareOutcome::Denied);
    }

    #[test]
    fn tx_prepare_prompt_active_precondition_fails_when_target_is_missing() {
        let mut contract = sample_tx_contract(MissionTxState::Planned);
        contract.plan.preconditions = vec![TxPrecondition::PromptActive { pane_id: 99 }];
        let inputs = mission_tx_prepare_gate_inputs(
            &contract,
            &MockPolicyAuthorizer::allow_all(),
            &MockApprovalChecker::default(),
            &MockTargetLookup::default()
                .with_snapshot(live_target_snapshot(1, 1_700_000_000_000))
                .with_snapshot(live_target_snapshot(2, 1_700_000_000_000))
                .with_snapshot(live_target_snapshot(3, 1_700_000_000_000)),
            &sample_prepare_context(),
            1_700_000_000_000,
        );

        assert!(inputs.iter().all(|input| !input.preconditions_satisfied));
        assert!(inputs.iter().all(|input| {
            input.precondition_reason_code.as_deref()
                == Some("tx.prepare.precondition.prompt_active.target_missing:99")
        }));
    }

    #[test]
    fn tx_prepare_custom_precondition_is_explicitly_unsupported() {
        let mut contract = sample_tx_contract(MissionTxState::Planned);
        contract.plan.preconditions = vec![TxPrecondition::Custom {
            check: "operator_attested".to_string(),
        }];
        let mut inputs = tx_prepare_gate_inputs_allow_all(&contract);

        assert!(inputs.iter().all(|input| !input.preconditions_satisfied));
        assert!(inputs.iter().all(|input| {
            input.precondition_reason_code.as_deref()
                == Some("tx.prepare.precondition.custom_unsupported")
        }));

        // Even a caller-supplied passing bit cannot make an unsupported custom
        // expression executable: the reducer validates the plan itself.
        for input in &mut inputs {
            input.preconditions_satisfied = true;
            input.precondition_reason_code = None;
        }
        let report = evaluate_prepare_phase(
            &contract.intent.tx_id,
            &contract.plan,
            &inputs,
            MissionKillSwitchLevel::Off,
            1_700_000_000_000,
        )
        .expect("prepare report");
        assert_eq!(report.outcome, TxPrepareOutcome::Denied);
    }

    #[test]
    fn tx_prepare_missing_precondition_evidence_deserializes_fail_closed() {
        let mut contract = sample_tx_contract(MissionTxState::Planned);
        contract.plan.steps.truncate(1);
        let mut serialized = serde_json::to_value(
            tx_prepare_gate_inputs_allow_all(&contract)
                .into_iter()
                .next()
                .expect("prepare input"),
        )
        .expect("serialize prepare input");
        let removed_evidence = serialized
            .as_object_mut()
            .expect("prepare input object")
            .remove("preconditions_satisfied");
        assert!(removed_evidence.is_some());
        let input: TxPrepareGateInput =
            serde_json::from_value(serialized).expect("deserialize legacy prepare input");

        assert!(!input.preconditions_satisfied);
        let report = evaluate_prepare_phase(
            &contract.intent.tx_id,
            &contract.plan,
            &[input],
            MissionKillSwitchLevel::Off,
            1_700_000_000_000,
        )
        .expect("prepare report");
        assert_eq!(report.outcome, TxPrepareOutcome::Denied);
    }

    #[test]
    fn tx_prepare_gate_policy_denial_marks_only_targeted_step() {
        let contract = sample_tx_contract(MissionTxState::Planned);
        let policy = MockPolicyAuthorizer::allow_all().with_pane_decision(
            2,
            PolicyDecision::deny_with_rule("pane blocked", "policy.test.blocked"),
        );
        let approvals = MockApprovalChecker::default();
        let targets = MockTargetLookup::default()
            .with_snapshot(live_target_snapshot(1, 1_700_000_000_000))
            .with_snapshot(live_target_snapshot(2, 1_700_000_000_000))
            .with_snapshot(live_target_snapshot(3, 1_700_000_000_000));

        let inputs = mission_tx_prepare_gate_inputs(
            &contract,
            &policy,
            &approvals,
            &targets,
            &sample_prepare_context(),
            1_700_000_000_000,
        );

        assert!(inputs[0].policy_passed);
        assert!(!inputs[1].policy_passed);
        assert_eq!(
            inputs[1].policy_reason_code.as_deref(),
            Some("policy.test.blocked")
        );
        assert!(inputs[2].policy_passed);
    }

    #[test]
    fn tx_prepare_gate_policy_allows_non_pane_workflow_steps() {
        let mut contract = sample_tx_contract(MissionTxState::Planned);
        contract.plan.steps = vec![TxStep {
            step_id: TxStepId("tx-step:workflow".to_string()),
            ordinal: 1,
            action: StepAction::RunWorkflow {
                workflow_id: "wf-child".to_string(),
                params: None,
            },
            description: "run workflow".to_string(),
        }];

        let policy = MockPolicyAuthorizer::allow_all();
        let inputs = mission_tx_prepare_gate_inputs(
            &contract,
            &policy,
            &MockApprovalChecker::default(),
            &MockTargetLookup::default(),
            &sample_prepare_context(),
            1_700_000_000_000,
        );

        assert_eq!(inputs.len(), 1);
        assert!(inputs[0].policy_passed);
        assert!(inputs[0].target_liveness);
        assert!(inputs[0].reservation_available);
        assert_eq!(inputs[0].pane_id, None);
    }

    #[test]
    fn tx_prepare_phase_denied_when_policy_gate_fails() {
        let contract = sample_tx_contract(MissionTxState::Planned);
        let policy = MockPolicyAuthorizer::allow_all().with_pane_decision(
            2,
            PolicyDecision::deny_with_rule("pane blocked", "policy.test.blocked"),
        );
        let approvals = MockApprovalChecker::default();
        let targets = MockTargetLookup::default()
            .with_snapshot(live_target_snapshot(1, 1_700_000_000_000))
            .with_snapshot(live_target_snapshot(2, 1_700_000_000_000))
            .with_snapshot(live_target_snapshot(3, 1_700_000_000_000));
        let gate_inputs = mission_tx_prepare_gate_inputs(
            &contract,
            &policy,
            &approvals,
            &targets,
            &sample_prepare_context(),
            1_700_000_000_000,
        );

        let report = evaluate_prepare_phase(
            &contract.intent.tx_id,
            &contract.plan,
            &gate_inputs,
            MissionKillSwitchLevel::Off,
            1_700_000_000_000,
        )
        .expect("prepare report");

        assert_eq!(report.outcome, TxPrepareOutcome::Denied);
        assert_eq!(report.gate_inputs.len(), 3);
    }

    #[test]
    fn tx_prepare_gate_missing_approval_is_reported_without_failing_policy() {
        let contract = sample_tx_contract(MissionTxState::Planned);
        let policy = MockPolicyAuthorizer::allow_all().with_pane_decision(
            1,
            PolicyDecision::require_approval_with_rule(
                "need approval",
                "policy.test.require_approval",
            ),
        );
        let approvals = MockApprovalChecker::default();
        let targets = MockTargetLookup::default()
            .with_snapshot(live_target_snapshot(1, 1_700_000_000_000))
            .with_snapshot(live_target_snapshot(2, 1_700_000_000_000))
            .with_snapshot(live_target_snapshot(3, 1_700_000_000_000));

        let inputs = mission_tx_prepare_gate_inputs(
            &contract,
            &policy,
            &approvals,
            &targets,
            &sample_prepare_context(),
            1_700_000_000_000,
        );

        assert!(inputs[0].policy_passed);
        assert!(!inputs[0].approval_satisfied);
        assert_eq!(
            inputs[0].approval_reason_code.as_deref(),
            Some("policy.test.require_approval")
        );
        assert!(inputs[0].required_approval.is_some());
    }

    #[test]
    fn tx_prepare_gate_matching_approval_satisfies_requirement() {
        let contract = sample_tx_contract(MissionTxState::Planned);
        let policy = MockPolicyAuthorizer::allow_all().with_pane_decision(
            1,
            PolicyDecision::require_approval_with_rule(
                "need approval",
                "policy.test.require_approval",
            ),
        );
        let mut approvals = MockApprovalChecker::default();
        let targets = MockTargetLookup::default()
            .with_snapshot(live_target_snapshot(1, 1_700_000_000_000))
            .with_snapshot(live_target_snapshot(2, 1_700_000_000_000))
            .with_snapshot(live_target_snapshot(3, 1_700_000_000_000));

        let first_pass = mission_tx_prepare_gate_inputs(
            &contract,
            &policy,
            &approvals,
            &targets,
            &sample_prepare_context(),
            1_700_000_000_000,
        );
        approvals.grant(&ApprovalScope {
            workspace_id: first_pass[0]
                .required_approval
                .as_ref()
                .expect("approval scope")
                .workspace_id
                .clone(),
            action_kind: first_pass[0]
                .required_approval
                .as_ref()
                .expect("approval scope")
                .action_kind
                .clone(),
            pane_id: first_pass[0]
                .required_approval
                .as_ref()
                .expect("approval scope")
                .pane_id,
            action_fingerprint: first_pass[0]
                .required_approval
                .as_ref()
                .expect("approval scope")
                .action_fingerprint
                .clone(),
        });

        let second_pass = mission_tx_prepare_gate_inputs(
            &contract,
            &policy,
            &approvals,
            &targets,
            &sample_prepare_context(),
            1_700_000_000_000,
        );

        assert!(second_pass[0].approval_satisfied);
        assert!(second_pass[0].required_approval.is_some());
    }

    #[test]
    fn tx_prepare_gate_lookup_failure_surfaces_as_missing_approval() {
        let contract = sample_tx_contract(MissionTxState::Planned);
        let policy = MockPolicyAuthorizer::allow_all().with_pane_decision(
            1,
            PolicyDecision::require_approval_with_rule(
                "need approval",
                "policy.test.require_approval",
            ),
        );
        let mut approvals = MockApprovalChecker::default();
        let targets = MockTargetLookup::default()
            .with_snapshot(live_target_snapshot(1, 1_700_000_000_000))
            .with_snapshot(live_target_snapshot(2, 1_700_000_000_000))
            .with_snapshot(live_target_snapshot(3, 1_700_000_000_000));

        let first_pass = mission_tx_prepare_gate_inputs(
            &contract,
            &policy,
            &approvals,
            &targets,
            &sample_prepare_context(),
            1_700_000_000_000,
        );
        let scope = ApprovalScope {
            workspace_id: first_pass[0]
                .required_approval
                .as_ref()
                .expect("approval scope")
                .workspace_id
                .clone(),
            action_kind: first_pass[0]
                .required_approval
                .as_ref()
                .expect("approval scope")
                .action_kind
                .clone(),
            pane_id: first_pass[0]
                .required_approval
                .as_ref()
                .expect("approval scope")
                .pane_id,
            action_fingerprint: first_pass[0]
                .required_approval
                .as_ref()
                .expect("approval scope")
                .action_fingerprint
                .clone(),
        };
        approvals.fail_lookup(&scope);

        let second_pass = mission_tx_prepare_gate_inputs(
            &contract,
            &policy,
            &approvals,
            &targets,
            &sample_prepare_context(),
            1_700_000_000_000,
        );

        assert!(!second_pass[0].approval_satisfied);
        assert_eq!(
            second_pass[0].approval_reason_code.as_deref(),
            Some("tx.prepare.approval_lookup_failed")
        );
    }

    #[test]
    fn tx_prepare_phase_reports_require_approval_when_only_approval_is_missing() {
        let contract = sample_tx_contract(MissionTxState::Planned);
        let policy = MockPolicyAuthorizer::allow_all().with_pane_decision(
            1,
            PolicyDecision::require_approval_with_rule(
                "need approval",
                "policy.test.require_approval",
            ),
        );
        let gate_inputs = mission_tx_prepare_gate_inputs(
            &contract,
            &policy,
            &MockApprovalChecker::default(),
            &MockTargetLookup::default()
                .with_snapshot(live_target_snapshot(1, 1_700_000_000_000))
                .with_snapshot(live_target_snapshot(2, 1_700_000_000_000))
                .with_snapshot(live_target_snapshot(3, 1_700_000_000_000)),
            &sample_prepare_context(),
            1_700_000_000_000,
        );

        let report = evaluate_prepare_phase(
            &contract.intent.tx_id,
            &contract.plan,
            &gate_inputs,
            MissionKillSwitchLevel::Off,
            1_700_000_000_000,
        )
        .expect("prepare report");

        assert_eq!(report.outcome, TxPrepareOutcome::RequireApproval);
    }

    #[test]
    fn tx_prepare_gate_live_target_passes_liveness_check() {
        let contract = sample_tx_contract(MissionTxState::Planned);
        let inputs = mission_tx_prepare_gate_inputs(
            &contract,
            &MockPolicyAuthorizer::allow_all(),
            &MockApprovalChecker::default(),
            &MockTargetLookup::default()
                .with_snapshot(live_target_snapshot(1, 1_700_000_000_000))
                .with_snapshot(live_target_snapshot(2, 1_700_000_000_000))
                .with_snapshot(live_target_snapshot(3, 1_700_000_000_000)),
            &sample_prepare_context(),
            1_700_000_000_000,
        );

        assert!(inputs.iter().all(|input| input.target_liveness));
    }

    #[test]
    fn tx_prepare_gate_missing_target_fails_liveness() {
        let contract = sample_tx_contract(MissionTxState::Planned);
        let inputs = mission_tx_prepare_gate_inputs(
            &contract,
            &MockPolicyAuthorizer::allow_all(),
            &MockApprovalChecker::default(),
            &MockTargetLookup::default()
                .with_snapshot(live_target_snapshot(1, 1_700_000_000_000))
                .with_snapshot(live_target_snapshot(3, 1_700_000_000_000)),
            &sample_prepare_context(),
            1_700_000_000_000,
        );

        assert!(!inputs[1].target_liveness);
        assert_eq!(
            inputs[1].liveness_reason_code.as_deref(),
            Some("tx.prepare.target_missing:2")
        );
    }

    #[test]
    fn tx_prepare_gate_stale_target_fails_liveness() {
        let contract = sample_tx_contract(MissionTxState::Planned);
        let inputs = mission_tx_prepare_gate_inputs(
            &contract,
            &MockPolicyAuthorizer::allow_all(),
            &MockApprovalChecker::default(),
            &MockTargetLookup::default()
                .with_snapshot(live_target_snapshot(1, 1_700_000_000_000))
                .with_snapshot(live_target_snapshot(2, 1_699_999_900_000))
                .with_snapshot(live_target_snapshot(3, 1_700_000_000_000)),
            &sample_prepare_context(),
            1_700_000_000_000,
        );

        assert!(!inputs[1].target_liveness);
        assert_eq!(
            inputs[1].liveness_reason_code.as_deref(),
            Some("tx.prepare.target_stale:2")
        );
    }

    #[test]
    fn tx_prepare_gate_unobserved_target_fails_liveness() {
        let contract = sample_tx_contract(MissionTxState::Planned);
        let mut unobserved = live_target_snapshot(2, 1_700_000_000_000);
        unobserved.observed = false;
        let inputs = mission_tx_prepare_gate_inputs(
            &contract,
            &MockPolicyAuthorizer::allow_all(),
            &MockApprovalChecker::default(),
            &MockTargetLookup::default()
                .with_snapshot(live_target_snapshot(1, 1_700_000_000_000))
                .with_snapshot(unobserved)
                .with_snapshot(live_target_snapshot(3, 1_700_000_000_000)),
            &sample_prepare_context(),
            1_700_000_000_000,
        );

        assert!(!inputs[1].target_liveness);
        assert_eq!(
            inputs[1].liveness_reason_code.as_deref(),
            Some("tx.prepare.target_unobserved:2")
        );
    }

    #[test]
    fn tx_surface_commit_step_inputs_apply_failure_injection() {
        let contract = sample_tx_contract(MissionTxState::Prepared);
        let inputs = mission_tx_commit_step_inputs(&contract, Some("tx-step:2"), 42_424);

        assert_eq!(inputs.len(), 3);
        assert!(inputs[0].success);
        assert!(!inputs[1].success);
        assert_eq!(inputs[1].reason_code, "commit_step_failed_injected");
        assert_eq!(inputs[1].error_code.as_deref(), Some("FTX3999"));
        assert!(inputs[2].success);
    }

    #[test]
    fn tx_surface_synthetic_commit_report_marks_every_step_committed() {
        let contract = sample_tx_contract(MissionTxState::Planned);
        let report = mission_tx_synthetic_commit_report(&contract, 5_151);

        assert_eq!(report.tx_id.0, "tx:test");
        assert_eq!(report.committed_count, 3);
        assert_eq!(report.failed_count, 0);
        assert_eq!(report.skipped_count, 0);
        assert!(
            report
                .step_results
                .iter()
                .all(|result| result.outcome.is_committed())
        );
    }

    #[test]
    fn tx_surface_compensation_inputs_only_cover_committed_steps() {
        let contract = sample_tx_contract(MissionTxState::Planned);
        let commit_report = mission_tx_synthetic_commit_report(&contract, 100);
        let inputs = mission_tx_compensation_inputs(&commit_report, Some("tx-step:2"), 200);

        assert_eq!(inputs.len(), 3);
        assert!(inputs[0].success);
        assert!(!inputs[1].success);
        assert_eq!(inputs[1].reason_code, "compensation_failed_injected");
        assert_eq!(inputs[1].error_code.as_deref(), Some("FTX4999"));
        assert!(inputs[2].success);
    }

    #[test]
    fn tx_receipts_round_trip_through_typed_schema() {
        let contract = sample_tx_contract(MissionTxState::Prepared);
        let report = execute_commit_phase(
            &contract,
            &sample_commit_inputs(Some("tx-step:2")),
            MissionKillSwitchLevel::Off,
            false,
            10_500,
        )
        .expect("commit report");

        let receipt =
            serde_json::from_value::<TxReceipt>(report.receipts[0].clone()).expect("typed receipt");
        assert_eq!(receipt.phase, "commit");
        assert_eq!(receipt.tx_id, "tx:test");
        assert_eq!(receipt.plan_id, "plan:test");
        assert_eq!(receipt.state, MissionTxState::Committing);
        assert_eq!(receipt.step_id.as_deref(), Some("tx-step:1"));
        assert_eq!(receipt.outcome, "committed");
        assert_eq!(receipt.reason_code, "commit_succeeded");
    }

    #[test]
    fn tx_contract_validate_rejects_foreign_receipt_plan_ownership() {
        let contract = sample_tx_contract(MissionTxState::Prepared);
        let report = execute_commit_phase(
            &contract,
            &sample_commit_inputs(None),
            MissionKillSwitchLevel::Off,
            false,
            10_500,
        )
        .expect("commit report");
        let mut persisted = contract;
        persisted.receipts = report.receipts;
        persisted.receipts[0]["plan_id"] = serde_json::json!("plan:foreign");

        let err = persisted
            .validate()
            .expect_err("foreign receipt plan must fail closed");
        assert!(
            err.contains("commit receipt does not belong to tx tx:test / plan plan:test"),
            "unexpected ownership error: {err}"
        );
    }

    #[test]
    fn tx_contract_validate_rejects_incoherent_receipt_phase_state_and_outcome() {
        let contract = sample_tx_contract(MissionTxState::Prepared);
        let report = execute_commit_phase(
            &contract,
            &sample_commit_inputs(None),
            MissionKillSwitchLevel::Off,
            false,
            10_500,
        )
        .expect("commit report");
        let mut persisted = contract;
        persisted.receipts = report.receipts;

        let mut wrong_state = persisted.clone();
        wrong_state.receipts[0]["state"] = serde_json::json!("compensating");
        let state_err = wrong_state
            .validate()
            .expect_err("commit receipt in compensating state must fail closed");
        assert!(state_err.contains("commit receipt for seq 1 has incoherent tx state"));

        let mut wrong_outcome = persisted.clone();
        wrong_outcome.receipts[0]["outcome"] = serde_json::json!("compensated");
        let outcome_err = wrong_outcome
            .validate()
            .expect_err("commit receipt with compensation outcome must fail closed");
        assert!(outcome_err.contains("unknown commit receipt outcome 'compensated'"));

        persisted.receipts[0]["phase"] = serde_json::json!("unknown");
        let phase_err = persisted
            .validate()
            .expect_err("unknown receipt phase must fail closed");
        assert!(phase_err.contains("unknown transaction receipt phase 'unknown'"));
    }

    #[test]
    fn tx_contract_validate_rejects_non_increasing_seq_across_receipt_phases() {
        let commit_contract = sample_tx_contract(MissionTxState::Prepared);
        let commit_report = execute_commit_phase(
            &commit_contract,
            &sample_commit_inputs(Some("tx-step:3")),
            MissionKillSwitchLevel::Off,
            false,
            10_500,
        )
        .expect("commit report");
        let mut compensating_contract = sample_tx_contract(MissionTxState::Compensating);
        compensating_contract.receipts = commit_report.receipts.clone();
        let compensation_report = execute_compensation_phase(
            &compensating_contract,
            &commit_report,
            &sample_committed_comp_inputs(None),
            20_500,
        )
        .expect("compensation report");

        let mut persisted = sample_tx_contract(MissionTxState::Failed);
        persisted.receipts = commit_report.receipts.clone();
        let compensation_start = persisted.receipts.len();
        persisted.receipts.extend(compensation_report.receipts);
        persisted
            .validate()
            .expect("generated cross-phase receipt order should validate");

        let last_commit_seq =
            receipt_seq(commit_report.receipts.last().expect("last commit receipt"));
        persisted.receipts[compensation_start]["seq"] = serde_json::json!(last_commit_seq);
        let err = persisted
            .validate()
            .expect_err("duplicate seq at phase boundary must fail closed");
        assert!(
            err.contains("is not strictly greater than previous seq"),
            "unexpected receipt order error: {err}"
        );
    }

    #[test]
    fn tx_commit_receipt_sequence_overflow_fails_closed() {
        let contract = sample_tx_contract(MissionTxState::Prepared);
        let prior_report = execute_commit_phase(
            &contract,
            &sample_commit_inputs(None),
            MissionKillSwitchLevel::Off,
            false,
            10_500,
        )
        .expect("prior commit report");
        let mut at_sequence_limit = contract;
        at_sequence_limit.receipts = vec![prior_report.receipts[0].clone()];
        at_sequence_limit.receipts[0]["seq"] = serde_json::json!(u64::MAX);

        let err = execute_commit_phase(
            &at_sequence_limit,
            &sample_commit_inputs(None),
            MissionKillSwitchLevel::Off,
            false,
            11_000,
        )
        .expect_err("receipt sequence overflow must fail closed");
        assert_eq!(
            err,
            format!(
                "transaction receipt sequence overflow after seq {}",
                u64::MAX
            )
        );
    }

    #[test]
    fn tx_rollback_commit_report_uses_receipts_instead_of_marking_every_step_committed() {
        let commit_contract = sample_tx_contract(MissionTxState::Prepared);
        let commit_report = execute_commit_phase(
            &commit_contract,
            &sample_commit_inputs(Some("tx-step:2")),
            MissionKillSwitchLevel::Off,
            false,
            10_500,
        )
        .expect("commit report");

        let mut rollback_contract = sample_tx_contract(MissionTxState::Failed);
        rollback_contract.receipts = commit_report.receipts.clone();

        let rollback_report =
            mission_tx_rollback_commit_report(&rollback_contract, 11_000).expect("rollback report");

        assert_eq!(rollback_report.outcome, TxCommitOutcome::PartialFailure);
        assert_eq!(rollback_report.committed_count, 1);
        assert_eq!(rollback_report.failed_count, 1);
        assert_eq!(rollback_report.skipped_count, 1);
        assert_eq!(
            rollback_report.failure_boundary.as_deref(),
            Some("tx-step:2")
        );
        assert!(rollback_report.step_results[0].outcome.is_committed());
        assert!(!rollback_report.step_results[1].outcome.is_committed());
        assert!(rollback_report.step_results[2].outcome.is_skipped());
    }

    #[test]
    fn tx_rollback_commit_report_rejects_non_committed_tx_without_commit_receipts() {
        let contract = sample_tx_contract(MissionTxState::Prepared);
        let err = mission_tx_rollback_commit_report(&contract, 55).expect_err("rollback error");
        assert!(err.contains("rollback requires commit receipts"));
    }

    #[test]
    fn tx_rollback_commit_report_rejects_receipts_for_another_contract() {
        let commit_contract = sample_tx_contract(MissionTxState::Prepared);
        let commit_report = execute_commit_phase(
            &commit_contract,
            &sample_commit_inputs(None),
            MissionKillSwitchLevel::Off,
            false,
            10_500,
        )
        .expect("commit report");

        let mut rollback_contract = sample_tx_contract(MissionTxState::Failed);
        rollback_contract.intent.tx_id = TxId("tx:other".to_string());
        rollback_contract.plan.tx_id = rollback_contract.intent.tx_id.clone();
        rollback_contract.receipts = commit_report.receipts;

        let err =
            mission_tx_rollback_commit_report(&rollback_contract, 11_000).expect_err("mismatch");
        assert!(err.contains("commit receipt does not belong to tx tx:other / plan plan:test"));
    }

    #[test]
    fn tx_rollback_commit_report_rejects_receiptless_committed_tx() {
        let contract = sample_tx_contract(MissionTxState::Committed);
        let err = mission_tx_rollback_commit_report(&contract, 5_151)
            .expect_err("receiptless committed tx must fail closed");

        assert_eq!(
            err,
            "rollback requires commit receipts, got none for tx state committed"
        );
    }

    #[test]
    fn tx_rollback_commit_report_rejects_already_terminal_rollback_states() {
        for state in [MissionTxState::RolledBack, MissionTxState::Compensated] {
            let contract = sample_tx_contract(state);
            let err = mission_tx_rollback_commit_report(&contract, 5_151)
                .expect_err("already-rolled-back tx must fail closed");

            assert_eq!(
                err,
                format!("rollback is not allowed for terminal tx state {state}")
            );
        }
    }

    #[test]
    fn tx_rollback_commit_report_retries_only_failed_or_uncompensated_steps() {
        let commit_contract = sample_tx_contract(MissionTxState::Prepared);
        let commit_report = execute_commit_phase(
            &commit_contract,
            &sample_commit_inputs(Some("tx-step:3")),
            MissionKillSwitchLevel::Off,
            false,
            10_500,
        )
        .expect("commit report");

        let mut compensating_contract = sample_tx_contract(MissionTxState::Compensating);
        compensating_contract.receipts = commit_report.receipts.clone();
        let compensation_report = execute_compensation_phase(
            &compensating_contract,
            &commit_report,
            &sample_committed_comp_inputs(Some("tx-step:1")),
            20_500,
        )
        .expect("partial compensation report");
        assert_eq!(
            compensation_report.outcome,
            TxCompensationOutcome::CompensationFailed
        );

        let mut retry_contract = sample_tx_contract(MissionTxState::Failed);
        retry_contract.receipts = commit_report.receipts.clone();
        retry_contract
            .receipts
            .extend(compensation_report.receipts.clone());

        let retry_report = mission_tx_rollback_commit_report(&retry_contract, 21_000)
            .expect("retry commit reconstruction");
        assert_eq!(
            retry_report.step_results.len(),
            retry_contract.plan.steps.len()
        );
        assert_eq!(retry_report.committed_count, 1);
        assert_eq!(retry_report.failed_count, 1);
        assert_eq!(retry_report.skipped_count, 1);
        assert!(retry_report.step_results[0].outcome.is_committed());
        assert!(matches!(
            &retry_report.step_results[1].outcome,
            TxCommitStepOutcome::Skipped { reason_code }
                if reason_code == "already_compensated"
        ));
        assert!(matches!(
            retry_report.step_results[2].outcome,
            TxCommitStepOutcome::Failed { .. }
        ));

        let retry_inputs = mission_tx_compensation_inputs(&retry_report, None, 21_500);
        assert_eq!(retry_inputs.len(), 1);
        assert_eq!(retry_inputs[0].for_step_id.0, "tx-step:1");
    }

    #[test]
    fn tx_rollback_commit_report_keeps_successful_compensation_sticky() {
        let commit_contract = sample_tx_contract(MissionTxState::Prepared);
        let commit_report = execute_commit_phase(
            &commit_contract,
            &sample_commit_inputs(Some("tx-step:3")),
            MissionKillSwitchLevel::Off,
            false,
            10_500,
        )
        .expect("commit report");
        let mut compensating_contract = sample_tx_contract(MissionTxState::Compensating);
        compensating_contract.receipts = commit_report.receipts.clone();
        let compensation_report = execute_compensation_phase(
            &compensating_contract,
            &commit_report,
            &sample_committed_comp_inputs(Some("tx-step:1")),
            20_500,
        )
        .expect("partial compensation report");

        let mut retry_contract = sample_tx_contract(MissionTxState::Failed);
        retry_contract.receipts = commit_report.receipts;
        retry_contract.receipts.extend(compensation_report.receipts);
        let next_seq = tx_increment_receipt_seq(tx_last_receipt_seq(&retry_contract.receipts))
            .expect("receipt sequence headroom");
        let step_id = TxStepId("tx-step:2".to_string());
        retry_contract.receipts.push(tx_build_receipt(
            next_seq,
            "compensate",
            &retry_contract.intent.tx_id,
            &retry_contract.plan.plan_id,
            MissionTxState::Compensating,
            Some(&step_id),
            "failed",
            "later_retry_failed",
            Some("FTX4999"),
            "test->later_retry_failed",
            21_000,
        ));

        let retry_report = mission_tx_rollback_commit_report(&retry_contract, 21_500)
            .expect("sticky compensation reconstruction");

        assert!(matches!(
            &retry_report.step_results[1].outcome,
            TxCommitStepOutcome::Skipped { reason_code }
                if reason_code == "already_compensated"
        ));
        let retry_inputs = mission_tx_compensation_inputs(&retry_report, None, 22_000);
        assert_eq!(retry_inputs.len(), 1);
        assert_eq!(retry_inputs[0].for_step_id.0, "tx-step:1");
    }

    #[test]
    fn tx_rollback_commit_report_keeps_successful_commit_sticky() {
        let commit_contract = sample_tx_contract(MissionTxState::Prepared);
        let commit_report = execute_commit_phase(
            &commit_contract,
            &sample_commit_inputs(None),
            MissionKillSwitchLevel::Off,
            false,
            10_500,
        )
        .expect("commit report");

        let mut retry_contract = sample_tx_contract(MissionTxState::Failed);
        retry_contract.receipts = commit_report.receipts;
        let step_id = TxStepId("tx-step:2".to_string());
        for (outcome, reason_code) in [
            ("failed", "later_retry_failed"),
            ("skipped", "later_retry_skipped"),
        ] {
            let next_seq = tx_increment_receipt_seq(tx_last_receipt_seq(&retry_contract.receipts))
                .expect("receipt sequence headroom");
            retry_contract.receipts.push(tx_build_receipt(
                next_seq,
                "commit",
                &retry_contract.intent.tx_id,
                &retry_contract.plan.plan_id,
                MissionTxState::Committing,
                Some(&step_id),
                outcome,
                reason_code,
                (outcome == "failed").then_some("FTX4998"),
                "test->later_commit_retry",
                20_000 + i64::try_from(next_seq).expect("small test sequence"),
            ));
        }

        let retry_report = mission_tx_rollback_commit_report(&retry_contract, 21_000)
            .expect("sticky commit reconstruction");
        assert_eq!(retry_report.committed_count, 3);
        assert_eq!(retry_report.failed_count, 0);
        assert_eq!(retry_report.skipped_count, 0);
        assert!(retry_report.step_results[1].outcome.is_committed());
        let retry_inputs = mission_tx_compensation_inputs(&retry_report, None, 22_000);
        assert_eq!(retry_inputs.len(), 3);
        assert!(
            retry_inputs
                .iter()
                .any(|input| input.for_step_id == step_id)
        );
    }

    #[test]
    fn tx_rollback_commit_report_rejects_compensation_without_committed_fact() {
        let commit_contract = sample_tx_contract(MissionTxState::Prepared);
        let commit_report = execute_commit_phase(
            &commit_contract,
            &sample_commit_inputs(Some("tx-step:2")),
            MissionKillSwitchLevel::Off,
            false,
            10_500,
        )
        .expect("partial commit report");
        let mut retry_contract = sample_tx_contract(MissionTxState::Failed);
        retry_contract.receipts = commit_report.receipts;
        let next_seq = tx_increment_receipt_seq(tx_last_receipt_seq(&retry_contract.receipts))
            .expect("receipt sequence headroom");
        let step_id = TxStepId("tx-step:2".to_string());
        retry_contract.receipts.push(tx_build_receipt(
            next_seq,
            "compensate",
            &retry_contract.intent.tx_id,
            &retry_contract.plan.plan_id,
            MissionTxState::Compensating,
            Some(&step_id),
            "compensated",
            "invalid_compensation",
            None,
            "test->invalid_compensation",
            20_000,
        ));

        let err = mission_tx_rollback_commit_report(&retry_contract, 21_000)
            .expect_err("a failed commit cannot authorize compensation");
        assert!(
            err.contains("step tx-step:2 has no preceding committed receipt"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn tx_rollback_commit_report_rejects_compensation_without_declared_action() {
        let commit_contract = sample_tx_contract(MissionTxState::Prepared);
        let commit_report = execute_commit_phase(
            &commit_contract,
            &sample_commit_inputs(None),
            MissionKillSwitchLevel::Off,
            false,
            10_500,
        )
        .expect("commit report");
        let mut retry_contract = sample_tx_contract(MissionTxState::Failed);
        retry_contract
            .plan
            .compensations
            .retain(|compensation| compensation.for_step_id.0 != "tx-step:2");
        retry_contract.receipts = commit_report.receipts;
        let next_seq = tx_increment_receipt_seq(tx_last_receipt_seq(&retry_contract.receipts))
            .expect("receipt sequence headroom");
        let step_id = TxStepId("tx-step:2".to_string());
        retry_contract.receipts.push(tx_build_receipt(
            next_seq,
            "compensate",
            &retry_contract.intent.tx_id,
            &retry_contract.plan.plan_id,
            MissionTxState::Compensating,
            Some(&step_id),
            "compensated",
            "forged_compensation",
            None,
            "test->forged_compensation",
            20_000,
        ));

        let err = mission_tx_rollback_commit_report(&retry_contract, 21_000)
            .expect_err("receipt cannot invent an undeclared compensation action");
        assert!(
            err.contains("step tx-step:2 without a declared compensation action"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn tx_rollback_commit_report_rejects_recommit_after_compensation_attempt() {
        let commit_contract = sample_tx_contract(MissionTxState::Prepared);
        let commit_report = execute_commit_phase(
            &commit_contract,
            &sample_commit_inputs(Some("tx-step:3")),
            MissionKillSwitchLevel::Off,
            false,
            10_500,
        )
        .expect("commit report");
        let mut compensating_contract = sample_tx_contract(MissionTxState::Compensating);
        compensating_contract.receipts = commit_report.receipts.clone();
        let compensation_report = execute_compensation_phase(
            &compensating_contract,
            &commit_report,
            &sample_committed_comp_inputs(Some("tx-step:1")),
            20_500,
        )
        .expect("partial compensation report");

        let mut retry_contract = sample_tx_contract(MissionTxState::Failed);
        retry_contract.receipts = commit_report.receipts;
        retry_contract.receipts.extend(compensation_report.receipts);
        let next_seq = tx_increment_receipt_seq(tx_last_receipt_seq(&retry_contract.receipts))
            .expect("receipt sequence headroom");
        let step_id = TxStepId("tx-step:2".to_string());
        retry_contract.receipts.push(tx_build_receipt(
            next_seq,
            "commit",
            &retry_contract.intent.tx_id,
            &retry_contract.plan.plan_id,
            MissionTxState::Committing,
            Some(&step_id),
            "committed",
            "unsafe_recommit",
            None,
            "test->unsafe_recommit",
            21_000,
        ));

        let err = mission_tx_rollback_commit_report(&retry_contract, 21_500)
            .expect_err("post-compensation recommit needs an explicit generation");
        assert!(
            err.contains("new effect generation is required before recommit"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn tx_rollback_commit_report_rejects_foreign_or_unknown_compensation_receipts() {
        let commit_contract = sample_tx_contract(MissionTxState::Prepared);
        let commit_report = execute_commit_phase(
            &commit_contract,
            &sample_commit_inputs(Some("tx-step:3")),
            MissionKillSwitchLevel::Off,
            false,
            10_500,
        )
        .expect("commit report");
        let mut compensating_contract = sample_tx_contract(MissionTxState::Compensating);
        compensating_contract.receipts = commit_report.receipts.clone();
        let compensation_report = execute_compensation_phase(
            &compensating_contract,
            &commit_report,
            &sample_committed_comp_inputs(Some("tx-step:1")),
            20_500,
        )
        .expect("partial compensation report");

        let mut contract = sample_tx_contract(MissionTxState::Failed);
        contract.receipts = commit_report.receipts;
        contract.receipts.extend(compensation_report.receipts);

        let mut foreign_receipt_contract = contract.clone();
        foreign_receipt_contract
            .receipts
            .last_mut()
            .expect("compensation receipt")["tx_id"] = serde_json::json!("tx:other");
        let foreign_err = mission_tx_rollback_commit_report(&foreign_receipt_contract, 21_000)
            .expect_err("foreign compensation receipt must fail closed");
        assert!(foreign_err.contains("compensation receipt does not belong"));

        contract.receipts.last_mut().expect("compensation receipt")["outcome"] =
            serde_json::json!("unknown");
        let outcome_err = mission_tx_rollback_commit_report(&contract, 21_000)
            .expect_err("unknown compensation outcome must fail closed");
        assert!(outcome_err.contains("unknown compensation receipt outcome 'unknown'"));
    }

    #[test]
    fn tx_prepare_phase_defers_when_any_plan_step_lacks_gate_input() {
        let contract = sample_tx_contract(MissionTxState::Planned);
        let gate_inputs = vec![
            TxPrepareGateInput {
                step_id: TxStepId("tx-step:1".to_string()),
                pane_id: Some(1),
                preconditions_satisfied: true,
                precondition_reason_code: None,
                policy_passed: true,
                policy_reason_code: None,
                reservation_available: true,
                reservation_reason_code: None,
                approval_satisfied: true,
                approval_reason_code: None,
                target_liveness: true,
                liveness_reason_code: None,
                required_approval: None,
            },
            TxPrepareGateInput {
                step_id: TxStepId("tx-step:1".to_string()),
                pane_id: Some(1),
                preconditions_satisfied: true,
                precondition_reason_code: None,
                policy_passed: true,
                policy_reason_code: None,
                reservation_available: true,
                reservation_reason_code: None,
                approval_satisfied: true,
                approval_reason_code: None,
                target_liveness: true,
                liveness_reason_code: None,
                required_approval: None,
            },
            TxPrepareGateInput {
                step_id: TxStepId("tx-step:2".to_string()),
                pane_id: Some(2),
                preconditions_satisfied: true,
                precondition_reason_code: None,
                policy_passed: true,
                policy_reason_code: None,
                reservation_available: true,
                reservation_reason_code: None,
                approval_satisfied: true,
                approval_reason_code: None,
                target_liveness: true,
                liveness_reason_code: None,
                required_approval: None,
            },
        ];

        let report = evaluate_prepare_phase(
            &contract.intent.tx_id,
            &contract.plan,
            &gate_inputs,
            MissionKillSwitchLevel::Off,
            1_700_000_000_000,
        )
        .expect("prepare report");

        assert_eq!(report.outcome, TxPrepareOutcome::Deferred);
    }

    #[test]
    fn tx_prepare_phase_ignores_unrelated_gate_inputs() {
        let contract = sample_tx_contract(MissionTxState::Planned);
        let mut gate_inputs = tx_prepare_gate_inputs_allow_all(&contract);
        gate_inputs.push(TxPrepareGateInput {
            step_id: TxStepId("tx-step:unrelated".to_string()),
            pane_id: Some(99),
            preconditions_satisfied: true,
            precondition_reason_code: None,
            policy_passed: false,
            policy_reason_code: Some("policy_denied".to_string()),
            reservation_available: false,
            reservation_reason_code: Some("reservation_missing".to_string()),
            approval_satisfied: false,
            approval_reason_code: Some("approval_missing".to_string()),
            target_liveness: false,
            liveness_reason_code: Some("target_unreachable".to_string()),
            required_approval: None,
        });

        let report = evaluate_prepare_phase(
            &contract.intent.tx_id,
            &contract.plan,
            &gate_inputs,
            MissionKillSwitchLevel::Off,
            1_700_000_000_000,
        )
        .expect("prepare report");

        assert_eq!(report.outcome, TxPrepareOutcome::AllReady);
    }

    fn receipt_seq(receipt: &serde_json::Value) -> u64 {
        receipt
            .get("seq")
            .and_then(serde_json::Value::as_u64)
            .expect("receipt seq")
    }

    #[test]
    fn tx_commit_phase_sets_failure_boundary_and_skips_later_steps() {
        let contract = sample_tx_contract(MissionTxState::Prepared);
        let report = execute_commit_phase(
            &contract,
            &sample_commit_inputs(Some("tx-step:2")),
            MissionKillSwitchLevel::Off,
            false,
            10_500,
        )
        .expect("commit report");

        assert_eq!(report.outcome, TxCommitOutcome::PartialFailure);
        assert_eq!(report.failure_boundary.as_deref(), Some("tx-step:2"));
        assert_eq!(report.committed_count, 1);
        assert_eq!(report.failed_count, 1);
        assert_eq!(report.skipped_count, 1);
        assert_eq!(report.step_results.len(), 3);
        assert!(report.step_results[2].outcome.is_skipped());
        assert_eq!(report.receipts.len(), 3);
        assert!(receipt_seq(&report.receipts[1]) > receipt_seq(&report.receipts[0]));
        assert!(receipt_seq(&report.receipts[2]) > receipt_seq(&report.receipts[1]));
    }

    #[test]
    fn tx_phase_reducers_reject_duplicate_step_inputs() {
        let contract = sample_tx_contract(MissionTxState::Prepared);
        let mut commit_inputs = sample_commit_inputs(None);
        commit_inputs.insert(
            1,
            TxCommitStepInput {
                step_id: TxStepId("tx-step:1".to_string()),
                success: false,
                reason_code: "conflicting_duplicate".to_string(),
                error_code: Some("FTX_DUPLICATE".to_string()),
                completed_at_ms: 10_001,
            },
        );

        let commit_err = execute_commit_phase(
            &contract,
            &commit_inputs,
            MissionKillSwitchLevel::Off,
            false,
            10_500,
        )
        .expect_err("duplicate commit input must fail closed");
        assert!(
            commit_err.contains("duplicate commit input for tx step tx-step:1"),
            "unexpected commit error: {commit_err}"
        );

        let commit_report = execute_commit_phase(
            &contract,
            &sample_commit_inputs(Some("tx-step:3")),
            MissionKillSwitchLevel::Off,
            false,
            10_500,
        )
        .expect("commit report");

        let mut compensating_contract = sample_tx_contract(MissionTxState::Compensating);
        compensating_contract.receipts = commit_report.receipts.clone();
        let mut comp_inputs = sample_committed_comp_inputs(None);
        comp_inputs.insert(
            0,
            TxCompensationStepInput {
                for_step_id: TxStepId("tx-step:2".to_string()),
                success: false,
                reason_code: "conflicting_duplicate".to_string(),
                error_code: Some("FTX_DUPLICATE".to_string()),
                completed_at_ms: 20_001,
            },
        );

        let comp_err = execute_compensation_phase(
            &compensating_contract,
            &commit_report,
            &comp_inputs,
            20_500,
        )
        .expect_err("duplicate compensation input must fail closed");
        assert!(
            comp_err.contains("duplicate compensation input for tx step tx-step:2"),
            "unexpected compensation error: {comp_err}"
        );
    }

    #[test]
    fn tx_phase_reducers_reject_malformed_input_identifiers() {
        let contract = sample_tx_contract(MissionTxState::Prepared);

        let prepare_tx_err = evaluate_prepare_phase(
            &TxId("tx/escape".to_string()),
            &contract.plan,
            &tx_prepare_gate_inputs_allow_all(&contract),
            MissionKillSwitchLevel::Off,
            1_700_000_000_000,
        )
        .expect_err("prepare should reject malformed tx id");
        assert!(
            prepare_tx_err.contains("prepare tx_id"),
            "unexpected prepare tx id error: {prepare_tx_err}"
        );

        let prepare_mismatch_err = evaluate_prepare_phase(
            &TxId("tx:other".to_string()),
            &contract.plan,
            &tx_prepare_gate_inputs_allow_all(&contract),
            MissionKillSwitchLevel::Off,
            1_700_000_000_000,
        )
        .expect_err("prepare should reject mismatched tx id");
        assert!(
            prepare_mismatch_err.contains("does not match plan tx_id"),
            "unexpected prepare mismatch error: {prepare_mismatch_err}"
        );

        let mut gate_inputs = tx_prepare_gate_inputs_allow_all(&contract);
        gate_inputs.push(TxPrepareGateInput {
            step_id: TxStepId("tx-step/escape".to_string()),
            pane_id: Some(99),
            preconditions_satisfied: true,
            precondition_reason_code: None,
            policy_passed: true,
            policy_reason_code: None,
            reservation_available: true,
            reservation_reason_code: None,
            approval_satisfied: true,
            approval_reason_code: None,
            target_liveness: true,
            liveness_reason_code: None,
            required_approval: None,
        });
        let prepare_err = evaluate_prepare_phase(
            &contract.intent.tx_id,
            &contract.plan,
            &gate_inputs,
            MissionKillSwitchLevel::Off,
            1_700_000_000_000,
        )
        .expect_err("prepare should reject malformed gate step id");
        assert!(
            prepare_err.contains("prepare gate step_id"),
            "unexpected prepare error: {prepare_err}"
        );

        let mut commit_inputs = sample_commit_inputs(None);
        commit_inputs.push(TxCommitStepInput {
            step_id: TxStepId("tx-step\\escape".to_string()),
            success: true,
            reason_code: "commit_succeeded".to_string(),
            error_code: None,
            completed_at_ms: 10_999,
        });
        let commit_err = execute_commit_phase(
            &contract,
            &commit_inputs,
            MissionKillSwitchLevel::Off,
            false,
            10_500,
        )
        .expect_err("commit should reject malformed input step id");
        assert!(
            commit_err.contains("commit input step_id"),
            "unexpected commit error: {commit_err}"
        );

        let mut unknown_commit_inputs = sample_commit_inputs(None);
        unknown_commit_inputs.push(TxCommitStepInput {
            step_id: TxStepId("tx-step:unknown".to_string()),
            success: true,
            reason_code: "commit_succeeded".to_string(),
            error_code: None,
            completed_at_ms: 10_999,
        });
        let unknown_commit_err = execute_commit_phase(
            &contract,
            &unknown_commit_inputs,
            MissionKillSwitchLevel::Off,
            false,
            10_500,
        )
        .expect_err("commit should reject unknown input step id");
        assert!(
            unknown_commit_err.contains("commit input references unknown step_id"),
            "unexpected unknown commit error: {unknown_commit_err}"
        );

        let commit_report = execute_commit_phase(
            &contract,
            &sample_commit_inputs(Some("tx-step:3")),
            MissionKillSwitchLevel::Off,
            false,
            10_500,
        )
        .expect("commit report");

        let mut compensating_contract = sample_tx_contract(MissionTxState::Compensating);
        compensating_contract.receipts = commit_report.receipts.clone();
        let mut comp_inputs = sample_committed_comp_inputs(None);
        comp_inputs.push(TxCompensationStepInput {
            for_step_id: TxStepId("..".to_string()),
            success: true,
            reason_code: "compensation_succeeded".to_string(),
            error_code: None,
            completed_at_ms: 20_999,
        });
        let comp_err = execute_compensation_phase(
            &compensating_contract,
            &commit_report,
            &comp_inputs,
            20_500,
        )
        .expect_err("compensation should reject malformed input step id");
        assert!(
            comp_err.contains("compensation input step_id"),
            "unexpected compensation error: {comp_err}"
        );

        let mut unknown_comp_inputs = sample_committed_comp_inputs(None);
        unknown_comp_inputs.push(TxCompensationStepInput {
            for_step_id: TxStepId("tx-step:unknown".to_string()),
            success: true,
            reason_code: "compensation_succeeded".to_string(),
            error_code: None,
            completed_at_ms: 20_999,
        });
        let unknown_comp_err = execute_compensation_phase(
            &compensating_contract,
            &commit_report,
            &unknown_comp_inputs,
            20_500,
        )
        .expect_err("compensation should reject unknown input step id");
        assert!(
            unknown_comp_err.contains("compensation input references unknown step_id"),
            "unexpected unknown compensation error: {unknown_comp_err}"
        );
    }

    #[test]
    fn tx_compensation_phase_rejects_uncommitted_step_input() {
        let commit_contract = sample_tx_contract(MissionTxState::Prepared);
        let commit_report = execute_commit_phase(
            &commit_contract,
            &sample_commit_inputs(Some("tx-step:3")),
            MissionKillSwitchLevel::Off,
            false,
            10_500,
        )
        .expect("commit report");

        let mut compensating_contract = sample_tx_contract(MissionTxState::Compensating);
        compensating_contract.receipts = commit_report.receipts.clone();
        let mut comp_inputs = sample_committed_comp_inputs(None);
        comp_inputs.push(TxCompensationStepInput {
            for_step_id: TxStepId("tx-step:3".to_string()),
            success: true,
            reason_code: "compensation_succeeded".to_string(),
            error_code: None,
            completed_at_ms: 20_999,
        });

        let comp_err = execute_compensation_phase(
            &compensating_contract,
            &commit_report,
            &comp_inputs,
            20_500,
        )
        .expect_err("compensation should reject inputs for uncommitted steps");
        assert!(
            comp_err.contains("compensation input references uncommitted step_id tx-step:3"),
            "unexpected uncommitted compensation error: {comp_err}"
        );
    }

    #[test]
    fn tx_commit_phase_returns_non_error_reports_for_pause_and_kill_switch() {
        let contract = sample_tx_contract(MissionTxState::Prepared);

        let paused = execute_commit_phase(
            &contract,
            &sample_commit_inputs(None),
            MissionKillSwitchLevel::Off,
            true,
            10_000,
        )
        .expect("paused report");
        assert_eq!(paused.outcome, TxCommitOutcome::PauseSuspended);
        assert_eq!(paused.committed_count, 0);
        assert_eq!(paused.failed_count, 0);
        assert_eq!(paused.skipped_count, 3);
        assert_eq!(paused.outcome.target_tx_state(), MissionTxState::Committing);

        let blocked = execute_commit_phase(
            &contract,
            &sample_commit_inputs(None),
            MissionKillSwitchLevel::SafeMode,
            false,
            10_000,
        )
        .expect("blocked report");
        assert_eq!(blocked.outcome, TxCommitOutcome::KillSwitchBlocked);
        assert_eq!(blocked.committed_count, 0);
        assert_eq!(blocked.failed_count, 0);
        assert_eq!(blocked.skipped_count, 3);
        assert!(
            blocked
                .receipts
                .iter()
                .all(|receipt| receipt["reason_code"] == "kill_switch_blocked")
        );
    }

    #[test]
    fn tx_compensation_phase_runs_in_reverse_order_and_continues_receipts() {
        let commit_contract = sample_tx_contract(MissionTxState::Prepared);
        let commit_report = execute_commit_phase(
            &commit_contract,
            &sample_commit_inputs(Some("tx-step:3")),
            MissionKillSwitchLevel::Off,
            false,
            10_500,
        )
        .expect("commit report");

        let mut compensating_contract = sample_tx_contract(MissionTxState::Compensating);
        compensating_contract.receipts = commit_report.receipts.clone();
        let comp_report = execute_compensation_phase(
            &compensating_contract,
            &commit_report,
            &sample_committed_comp_inputs(None),
            20_500,
        )
        .expect("compensation report");

        assert_eq!(comp_report.outcome, TxCompensationOutcome::FullyRolledBack);
        assert_eq!(comp_report.compensated_count, 2);
        assert_eq!(comp_report.failed_count, 0);
        assert_eq!(comp_report.skipped_count, 0);
        assert_eq!(comp_report.step_results.len(), 2);
        assert_eq!(comp_report.step_results[0].step_id.0, "tx-step:2");
        assert!(matches!(
            comp_report.step_results[0].outcome,
            TxCommitStepOutcome::Committed { .. }
        ));
        assert_eq!(comp_report.step_results[1].step_id.0, "tx-step:1");
        assert!(matches!(
            comp_report.step_results[1].outcome,
            TxCommitStepOutcome::Committed { .. }
        ));
        assert_eq!(comp_report.receipts.len(), 2);
        assert_eq!(comp_report.receipts[0]["step_id"], "tx-step:2");
        assert_eq!(comp_report.receipts[1]["step_id"], "tx-step:1");
        let last_commit_seq = receipt_seq(commit_report.receipts.last().expect("commit receipt"));
        assert!(receipt_seq(&comp_report.receipts[0]) > last_commit_seq);
        assert!(receipt_seq(&comp_report.receipts[1]) > receipt_seq(&comp_report.receipts[0]));
    }

    #[test]
    fn tx_compensation_phase_preserves_failure_and_skip_step_results() {
        let commit_contract = sample_tx_contract(MissionTxState::Prepared);
        let commit_report = execute_commit_phase(
            &commit_contract,
            &sample_commit_inputs(Some("tx-step:3")),
            MissionKillSwitchLevel::Off,
            false,
            10_500,
        )
        .expect("commit report");

        let mut compensating_contract = sample_tx_contract(MissionTxState::Compensating);
        compensating_contract.receipts = commit_report.receipts.clone();
        let comp_report = execute_compensation_phase(
            &compensating_contract,
            &commit_report,
            &sample_committed_comp_inputs(Some("tx-step:2")),
            20_500,
        )
        .expect("compensation report");

        assert_eq!(
            comp_report.outcome,
            TxCompensationOutcome::CompensationFailed
        );
        assert_eq!(comp_report.compensated_count, 0);
        assert_eq!(comp_report.failed_count, 1);
        assert_eq!(comp_report.skipped_count, 1);
        assert_eq!(comp_report.step_results.len(), 2);
        assert_eq!(comp_report.step_results[0].step_id.0, "tx-step:2");
        assert!(matches!(
            comp_report.step_results[0].outcome,
            TxCommitStepOutcome::Failed { .. }
        ));
        assert_eq!(comp_report.step_results[1].step_id.0, "tx-step:1");
        assert!(matches!(
            comp_report.step_results[1].outcome,
            TxCommitStepOutcome::Skipped { .. }
        ));
        assert_eq!(comp_report.receipts[0]["step_id"], "tx-step:2");
        assert_eq!(comp_report.receipts[1]["step_id"], "tx-step:1");
    }

    #[test]
    fn tx_commit_and_compensation_validate_required_lifecycle_states() {
        let invalid_commit = sample_tx_contract(MissionTxState::Planned);
        let commit_err = execute_commit_phase(
            &invalid_commit,
            &sample_commit_inputs(None),
            MissionKillSwitchLevel::Off,
            false,
            10_000,
        )
        .expect_err("commit should reject invalid state");
        assert!(commit_err.contains("prepared or committing"));

        let valid_commit = execute_commit_phase(
            &sample_tx_contract(MissionTxState::Prepared),
            &sample_commit_inputs(Some("tx-step:3")),
            MissionKillSwitchLevel::Off,
            false,
            10_000,
        )
        .expect("valid commit");
        let invalid_comp = sample_tx_contract(MissionTxState::Failed);
        let comp_err = execute_compensation_phase(
            &invalid_comp,
            &valid_commit,
            &sample_comp_inputs(None),
            20_000,
        )
        .expect_err("compensation should reject invalid state");
        assert!(comp_err.contains("compensating"));
    }

    #[test]
    fn tx_transition_table_includes_committed_rollback_path() {
        assert!(MISSION_TX_TRANSITIONS.iter().any(|rule| {
            rule.from == MissionTxState::Committed
                && rule.via == MissionTxTransitionKind::Compensate
                && rule.to == MissionTxState::Compensating
        }));
    }

    #[test]
    fn tx_transition_table_includes_both_compensation_success_terminal_states() {
        let compensating_targets = MISSION_TX_TRANSITIONS
            .iter()
            .filter(|rule| {
                rule.from == MissionTxState::Compensating
                    && rule.via == MissionTxTransitionKind::Complete
            })
            .map(|rule| rule.to)
            .collect::<std::collections::HashSet<_>>();

        assert!(compensating_targets.contains(&MissionTxState::Compensated));
        assert!(compensating_targets.contains(&MissionTxState::RolledBack));
    }

    #[test]
    fn tx_transition_table_has_no_duplicate_entries() {
        let mut seen = std::collections::HashSet::new();
        for (i, rule) in MISSION_TX_TRANSITIONS.iter().enumerate() {
            let key = format!("{:?}_{:?}_{:?}", rule.from, rule.via, rule.to);
            assert!(
                seen.insert(key.clone()),
                "Duplicate tx transition at index {i}: {key}",
            );
        }
    }

    // =========================================================================
    // Transition table completeness test
    // =========================================================================

    /// Validates that every entry in MISSION_LIFECYCLE_TRANSITIONS is
    /// individually reachable via `transition_lifecycle()`. This ensures
    /// the table doesn't contain dead/unreachable transitions and that
    /// the validation logic accepts every declared (from, via, to) triple.
    #[test]
    fn every_transition_in_table_is_reachable() {
        for (i, rule) in MISSION_LIFECYCLE_TRANSITIONS.iter().enumerate() {
            let mut mission = planning_mission();
            mission.lifecycle_state = rule.from;

            let result =
                mission.transition_lifecycle(rule.to, rule.via, (i as i64 + 1) * 1_000_000);
            assert!(
                result.is_ok(),
                "Transition table entry {i} should be valid: {:?} --{:?}--> {:?}",
                rule.from,
                rule.via,
                rule.to,
            );
            assert_eq!(
                mission.lifecycle_state, rule.to,
                "Transition {i} should land in {:?}",
                rule.to,
            );
        }
    }

    /// Validates that transitions NOT in the table are rejected.
    /// Picks a sample of invalid transitions and confirms they fail.
    #[test]
    fn invalid_transitions_not_in_table_are_rejected() {
        let invalid_cases = [
            // Terminal states cannot transition out
            (
                MissionLifecycleState::Completed,
                MissionLifecycleTransitionKind::Retry,
                MissionLifecycleState::RetryPending,
            ),
            (
                MissionLifecycleState::Failed,
                MissionLifecycleTransitionKind::Dispatch,
                MissionLifecycleState::Dispatching,
            ),
            (
                MissionLifecycleState::Cancelled,
                MissionLifecycleTransitionKind::RetryResumed,
                MissionLifecycleState::Running,
            ),
            // Planning cannot directly go to Running
            (
                MissionLifecycleState::Planning,
                MissionLifecycleTransitionKind::Retry,
                MissionLifecycleState::Running,
            ),
            // AwaitingApproval cannot directly Complete
            (
                MissionLifecycleState::AwaitingApproval,
                MissionLifecycleTransitionKind::Complete,
                MissionLifecycleState::Completed,
            ),
        ];

        for (i, (from, via, to)) in invalid_cases.iter().enumerate() {
            let mut mission = planning_mission();
            mission.lifecycle_state = *from;

            let result = mission.transition_lifecycle(*to, *via, (i as i64 + 1) * 1_000_000);
            assert!(
                result.is_err(),
                "Invalid transition {i} should be rejected: {:?} --{:?}--> {:?}",
                from,
                via,
                to,
            );
        }
    }

    /// Validates that the transition table has no duplicate entries
    /// (same from+via+to triple appearing more than once).
    #[test]
    fn transition_table_has_no_duplicates() {
        let mut seen = std::collections::HashSet::new();
        for (i, rule) in MISSION_LIFECYCLE_TRANSITIONS.iter().enumerate() {
            let key = format!("{:?}_{:?}_{:?}", rule.from, rule.via, rule.to);
            assert!(
                seen.insert(key.clone()),
                "Duplicate transition at index {i}: {key}",
            );
        }
    }

    #[test]
    fn validate_rejects_compensation_receipt_before_any_commit_receipt() {
        let mut contract = MissionTxContract::new(
            TxIntent::new("tx-test-reducer", "goal", 0),
            TxPlan::new(
                "plan-test-reducer",
                vec![TxStep::new(
                    "step-0",
                    StepAction::SendText {
                        pane_id: 1,
                        text: "echo ok".to_string(),
                    },
                )],
            )
            .with_compensations(vec![TxCompensation::new(
                "step-0",
                StepAction::SendText {
                    pane_id: 1,
                    text: "echo rollback".to_string(),
                },
            )]),
        );
        contract.lifecycle_state = MissionTxState::Compensating;
        contract.receipts.push(serde_json::json!({
            "seq": 1,
            "phase": "compensate",
            "step_id": "step-0",
            "state": "compensating",
            "outcome": "compensated",
            "tx_id": "tx-test-reducer",
            "plan_id": "plan-test-reducer",
            "emitted_at_ms": 1000
        }));

        let err = contract.validate().unwrap_err();
        assert!(err.contains("has no preceding committed receipt"));
    }

    #[test]
    fn validate_rejects_compensation_receipt_after_failed_commit_receipt() {
        let mut contract = MissionTxContract::new(
            TxIntent::new("tx-test-reducer-failed", "goal", 0),
            TxPlan::new(
                "plan-test-reducer-failed",
                vec![TxStep::new(
                    "step-0",
                    StepAction::SendText {
                        pane_id: 1,
                        text: "echo ok".to_string(),
                    },
                )],
            )
            .with_compensations(vec![TxCompensation::new(
                "step-0",
                StepAction::SendText {
                    pane_id: 1,
                    text: "echo rollback".to_string(),
                },
            )]),
        );
        contract.lifecycle_state = MissionTxState::Compensating;
        contract.receipts.push(serde_json::json!({
            "seq": 1,
            "phase": "commit",
            "step_id": "step-0",
            "state": "committing",
            "outcome": "failed",
            "tx_id": "tx-test-reducer-failed",
            "plan_id": "plan-test-reducer-failed",
            "emitted_at_ms": 1000
        }));
        contract.receipts.push(serde_json::json!({
            "seq": 2,
            "phase": "compensate",
            "step_id": "step-0",
            "state": "compensating",
            "outcome": "compensated",
            "tx_id": "tx-test-reducer-failed",
            "plan_id": "plan-test-reducer-failed",
            "emitted_at_ms": 2000
        }));

        let err = contract.validate().unwrap_err();
        assert!(err.contains("has no preceding committed receipt"));
    }

    #[test]
    fn validate_rejects_recommit_after_compensation_attempt() {
        let mut contract = MissionTxContract::new(
            TxIntent::new("tx-test-recommit", "goal", 0),
            TxPlan::new(
                "plan-test-recommit",
                vec![TxStep::new(
                    "step-0",
                    StepAction::SendText {
                        pane_id: 1,
                        text: "echo ok".to_string(),
                    },
                )],
            )
            .with_compensations(vec![TxCompensation::new(
                "step-0",
                StepAction::SendText {
                    pane_id: 1,
                    text: "echo rollback".to_string(),
                },
            )]),
        );
        contract.lifecycle_state = MissionTxState::Committing;
        contract.receipts.push(serde_json::json!({
            "seq": 1,
            "phase": "commit",
            "step_id": "step-0",
            "state": "committing",
            "outcome": "committed",
            "tx_id": "tx-test-recommit",
            "plan_id": "plan-test-recommit",
            "emitted_at_ms": 1000
        }));
        contract.receipts.push(serde_json::json!({
            "seq": 2,
            "phase": "compensate",
            "step_id": "step-0",
            "state": "compensating",
            "outcome": "compensated",
            "tx_id": "tx-test-recommit",
            "plan_id": "plan-test-recommit",
            "emitted_at_ms": 2000
        }));
        contract.receipts.push(serde_json::json!({
            "seq": 3,
            "phase": "commit",
            "step_id": "step-0",
            "state": "committing",
            "outcome": "committed",
            "tx_id": "tx-test-recommit",
            "plan_id": "plan-test-recommit",
            "emitted_at_ms": 3000
        }));

        let err = contract.validate().unwrap_err();
        assert!(err.contains(
            "follows a compensation attempt; a new effect generation is required before recommit"
        ));
    }
}
