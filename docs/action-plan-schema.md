# ActionPlan/StepPlan Schema Design

**Bead:** wa-upg.2.1
**Author:** GrayGorge
**Date:** 2026-01-27
**Status:** Draft

## Overview

This document specifies the unified action plan schema for `wa` workflows. The goal is to provide a single, consistent representation for planned actions across all command paths (Robot Mode, workflows, dry-run preview) with:

- Deterministic serialization for stable hashing
- Idempotency tracking for safe replay
- Clear preconditions and verification steps
- Graceful failure handling

## Design Principles

1. **Determinism**: All serialization must be canonical (stable field ordering) for consistent hashing
2. **Composability**: Plans can be nested (steps containing sub-plans)
3. **Observability**: Every step has clear before/after verification
4. **Safety**: Explicit preconditions prevent partial execution
5. **Idempotency**: Content-addressed steps enable safe retry

## Rust Type Definitions

### Core Plan Structure

```rust
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

/// Content-addressed plan identifier
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PlanId(pub String);

impl PlanId {
    /// Create a plan ID from a hash
    pub fn from_hash(hash: &str) -> Self {
        Self(format!("plan:{hash}"))
    }
}
```

### Step Definition

```rust
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

/// The action to perform in a step
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
    ReleaseLock {
        lock_name: String,
    },

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
    MarkEventHandled {
        event_id: i64,
    },

    /// Validate an approval token
    ValidateApproval {
        approval_code: String,
    },

    /// Execute a nested action plan
    NestedPlan {
        plan: Box<ActionPlan>,
    },

    /// Custom action with arbitrary payload
    Custom {
        action_type: String,
        payload: serde_json::Value,
    },
}

/// Condition to wait for (re-uses existing WaitCondition)
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

    /// Wait for external signal
    External {
        key: String,
    },
}
```

### Preconditions

```rust
/// A condition that must be satisfied before execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Precondition {
    /// Pane must exist and be accessible
    PaneExists {
        pane_id: u64,
    },

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
    LockHeld {
        lock_name: String,
    },

    /// A lock must be available
    LockAvailable {
        lock_name: String,
    },

    /// An approval must be valid
    ApprovalValid {
        scope: ApprovalScopeRef,
    },

    /// Previous step must have succeeded
    StepCompleted {
        step_id: IdempotencyKey,
    },

    /// Custom precondition with expression
    Custom {
        name: String,
        expression: String,
    },
}

/// Reference to an approval scope
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalScopeRef {
    pub workspace_id: String,
    pub action_kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<u64>,
}
```

### Verification

```rust
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
    Custom {
        name: String,
        expression: String,
    },

    /// No verification needed (fire-and-forget)
    None,
}
```

### Failure Handling

```rust
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
    Fallback {
        steps: Vec<StepPlan>,
    },

    /// Require human intervention
    RequireApproval {
        summary: String,
    },
}
```

### Idempotency Key

```rust
/// Content-addressed key for idempotent step execution.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct IdempotencyKey(pub String);

impl IdempotencyKey {
    /// Create from a hash
    pub fn from_hash(hash: &str) -> Self {
        Self(format!("step:{hash}"))
    }

    /// Compute key for a step action
    pub fn for_action(
        workspace_id: &str,
        step_number: u32,
        action: &StepAction,
    ) -> Self {
        let canonical = canonical_action_string(workspace_id, step_number, action);
        let hash = sha256_hex(&canonical);
        Self::from_hash(&hash[..16]) // Use first 16 chars for brevity
    }
}
```

## Canonical Serialization

### Field Ordering

All structs use `#[serde(rename_all = "snake_case")]` for consistent naming. Field order in serialization follows declaration order, which is stable across compilations.

For hash computation, a **canonical form** is used:

1. Fields are sorted alphabetically by key name
2. Optional fields with `None` values are omitted
3. Empty collections (`Vec::is_empty()`) are omitted
4. Timestamps and metadata are excluded
5. Numbers use decimal representation (no scientific notation)
6. Strings are UTF-8 without BOM

### Version Field

```rust
/// Current schema version
pub const PLAN_SCHEMA_VERSION: u32 = 1;
```

The `plan_version` field enables forward compatibility:
- Version 1: Initial schema (this document)
- Readers should reject plans with `plan_version > SUPPORTED_VERSION`
- Writers should always emit the current version

### Excluded Fields (Not Part of Hash)

The following fields are excluded from hash computation:
- `created_at` - timestamps change on regeneration
- `metadata` - auxiliary data that doesn't affect execution
- `PlanId` and `IdempotencyKey` - they're derived from the hash

## Plan Hash Derivation

### Algorithm

```rust
/// Compute a deterministic hash for an ActionPlan.
pub fn compute_plan_hash(plan: &ActionPlan) -> String {
    let canonical = canonical_plan_string(plan);
    format!("sha256:{}", sha256_hex(&canonical)[..32])
}

fn canonical_plan_string(plan: &ActionPlan) -> String {
    let mut parts = Vec::new();

    // Version
    parts.push(format!("v={}", plan.plan_version));

    // Workspace scope
    parts.push(format!("ws={}", plan.workspace_id));

    // Title
    parts.push(format!("title={}", plan.title));

    // Steps (in order)
    for (i, step) in plan.steps.iter().enumerate() {
        parts.push(format!("step[{}]={}", i, canonical_step_string(step)));
    }

    // Preconditions (sorted)
    let mut precond_strs: Vec<_> = plan.preconditions
        .iter()
        .map(canonical_precondition_string)
        .collect();
    precond_strs.sort();
    for (i, p) in precond_strs.iter().enumerate() {
        parts.push(format!("precond[{}]={}", i, p));
    }

    // On-failure (if set)
    if let Some(on_failure) = &plan.on_failure {
        parts.push(format!("on_failure={}", canonical_on_failure_string(on_failure)));
    }

    parts.join("|")
}

fn canonical_step_string(step: &StepPlan) -> String {
    let mut parts = Vec::new();

    parts.push(format!("n={}", step.step_number));
    parts.push(format!("action={}", canonical_action_string_inner(&step.action)));
    parts.push(format!("desc={}", step.description));
    parts.push(format!("idempotent={}", step.idempotent));

    if let Some(timeout) = step.timeout_ms {
        parts.push(format!("timeout={}", timeout));
    }

    // ... similar for preconditions, verification, on_failure

    parts.join(",")
}
```

### Hash Format

Plan hashes use the format: `sha256:<first-32-hex-chars>`

Example: `sha256:a1b2c3d4e5f67890a1b2c3d4e5f67890`

Step idempotency keys use: `step:<first-16-hex-chars>`

Example: `step:a1b2c3d4e5f67890`

## Rendering

### TTY Progressive Disclosure

For terminal output, use a hierarchical format with expandable sections:

```
Plan: Recover rate-limited agent
  ID: plan:a1b2c3d4e5f67890a1b2c3d4e5f67890
  Steps: 3
  Workspace: /project

  Preconditions:
    [PASS] Pane 0 exists
    [PASS] Agent detected: claude-code

  Steps:
    1. [PENDING] Send /compact command
       Action: send_text to pane 0
       Verification: wait for "compaction complete"

    2. [PENDING] Wait for idle
       Action: wait_for pane_idle (5000ms threshold)

    3. [PENDING] Resume work
       Action: send_text "/continue" to pane 0

  On Failure: abort
```

Flags to control detail level:
- `--verbose`: Show full action payloads
- `--quiet`: Show only step numbers and status
- `--json`: Full JSON output

### JSON Fully Structured

JSON output includes all fields with consistent structure:

```json
{
  "plan_version": 1,
  "plan_id": "plan:a1b2c3d4e5f67890a1b2c3d4e5f67890",
  "title": "Recover rate-limited agent",
  "workspace_id": "/project",
  "created_at": 1706385600000,
  "steps": [
    {
      "step_number": 1,
      "step_id": "step:deadbeef12345678",
      "action": {
        "type": "send_text",
        "pane_id": 0,
        "text": "/compact"
      },
      "description": "Send /compact command",
      "preconditions": [],
      "verification": {
        "strategy": {
          "type": "pattern_match",
          "rule_id": "core.claude:compaction_complete"
        },
        "timeout_ms": 60000
      },
      "on_failure": null,
      "timeout_ms": 120000,
      "idempotent": true
    }
  ],
  "preconditions": [
    {
      "type": "pane_exists",
      "pane_id": 0
    }
  ],
  "on_failure": {
    "strategy": "abort"
  }
}
```

### TOON Format

For token-efficient AI-to-AI communication, plans can be serialized in TOON format using the existing `toon_rust` infrastructure:

```rust
impl toon_rust::ToToon for ActionPlan {
    fn to_toon(&self) -> toon_rust::ToonValue {
        // Compact representation optimized for LLM token consumption
        // 40-60% smaller than equivalent JSON
    }
}
```

## Integration Points

### Existing Code Alignment

| New Type | Existing Type | Relationship |
|----------|---------------|--------------|
| `ActionPlan` | `DryRunReport` | ActionPlan is richer; DryRunReport can be derived |
| `StepPlan` | `PlannedAction` | StepPlan has preconditions, verification; PlannedAction is simpler |
| `StepAction` | `ActionType` | StepAction is tagged enum with payloads; ActionType is marker enum |
| `WaitCondition` | `workflows::WaitCondition` | Same semantics, unified definition |
| `OnFailure` | `workflows::StepResult` | OnFailure specifies policy; StepResult reports outcome |
| `IdempotencyKey` | `approval::fingerprint_for_input` | Same hashing approach, step-scoped |

### Migration Path

1. Introduce new types in `wa_core::plan` module
2. Add `From<ActionPlan> for DryRunReport` for backwards compatibility
3. Update workflow engine to emit `ActionPlan` for dry-run
4. Add `--plan` flag to robot commands to output full plan
5. Deprecate direct `DryRunReport` usage over time

## Future Considerations

- **Plan persistence**: Store plans in SQLite for replay/audit
- **Plan diff**: Compare two plans to show semantic differences
- **Plan compose**: Merge multiple plans with dependency resolution
- **Plan approval**: Require human approval for entire plans, not just individual actions
- **Distributed execution**: Plan partitioning for multi-host scenarios

## References

- `crates/wa-core/src/dry_run.rs` - Existing dry-run infrastructure
- `crates/wa-core/src/workflows.rs` - Workflow execution engine
- `crates/wa-core/src/approval.rs` - Approval and fingerprinting patterns
- `crates/wa-core/src/policy.rs` - Policy evaluation
