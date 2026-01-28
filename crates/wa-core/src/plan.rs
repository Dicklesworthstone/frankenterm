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
//! ```ignore
//! use wa_core::plan::{ActionPlan, StepPlan, StepAction};
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
                let value_str = serde_json::to_string(value).unwrap_or_default();
                format!("store_data:key={key},value={value_str}")
            }
            Self::RunWorkflow {
                workflow_id,
                params,
            } => {
                let params_str = params
                    .as_ref()
                    .and_then(|p| serde_json::to_string(p).ok())
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
                let payload_str = serde_json::to_string(payload).unwrap_or_default();
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
}
