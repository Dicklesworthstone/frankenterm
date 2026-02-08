# ActionPlan/StepPlan Schema Design

**Bead:** wa-upg.2.1
**Author:** GrayGorge
**Date:** 2026-01-27
**Status:** Living spec (updated through wa-upg.2.7)

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

## Mental Model (ActionPlan vs StepPlan)

**ActionPlan** is the immutable, deterministic description of intended work. It is created *before* any side effects, persisted, and used for previews, approvals, and audit.

**StepPlan** is the smallest unit of execution. Each step:

- declares its **action** (what we will do),
- declares **preconditions** (what must be true before the action),
- declares **verification** (how we know it worked),
- declares **failure handling** (what to do if verification fails),
- carries an **idempotency key** (so retries do not double-apply).

Plan-level preconditions apply to the entire plan. Step-level preconditions apply only to that step. Step-level `on_failure` overrides plan-level `on_failure`. Steps are **ordered and numbered**; plan execution is always sequential unless an explicit step expresses parallelism (not part of v1).

## Execution Lifecycle

1. **Build**: workflow or command constructs an ActionPlan (often via `ActionPlan::builder`).
2. **Validate**: plan validation ensures step numbering and references are consistent.
3. **Hash**: canonical hash is computed and stored in `plan_id` for approvals/idempotency.
4. **Persist**: the plan is recorded before any side effects.
5. **Execute**: each StepPlan is applied in order with precondition checks.
6. **Verify**: verification strategy runs; outcomes are logged to step logs.
7. **Finalize**: success or failure is recorded; plan/step logs remain durable.

This lifecycle is what enables dry-run previews to be truthful and enables restart/resume without double-applying actions.

## Plan Hashing and Approvals

The plan hash is derived from a **canonical string** and excludes volatile data (e.g., `created_at`, `metadata`). Canonicalization currently includes:

- `plan_version`, `workspace_id`, `title`
- ordered step canonical strings
- sorted global preconditions
- optional plan-level `on_failure`

Each StepPlan has its own canonical string that includes the action, preconditions, verification, and on-failure. Step idempotency keys are derived from a stable canonical representation of the action plus the step number. When adding fields or variants, ensure:

- canonical strings are updated
- tests for canonical stability are extended
- hashes remain stable for semantically identical plans

Approvals should bind to the plan hash (or the derived `plan_id`) so the approved plan cannot be swapped without invalidating the approval.

## Prepare/Commit UX + Plan-Hash Binding (Design)

This section specifies the **prepare/commit** flow for plan-hash-bound approvals. The goal is to prevent TOCTOU and confused-deputy mistakes while keeping the UX understandable for humans and structured for robots/MCP.

For the user-facing walkthrough and troubleshooting guide, see `docs/approvals.md`.

### When to Use Prepare/Commit

Use prepare/commit whenever an action can mutate a pane, run workflows, or otherwise perform a side effect. Concretely:

- **Required** when policy returns `require_approval`.
- **Recommended** when you want a deterministic preview before executing (even if approval is not required).
- **Default mental model** for any potentially risky or irreversible action.

If you are ever unsure, prefer `prepare` first. It is safe, shows exactly what will run, and produces the plan hash needed for approval binding.

### Plan Hash Mental Model

The plan hash is a digest of the **canonical** ActionPlan representation: actions, preconditions, verification steps, and on-failure behavior. It intentionally excludes volatile fields (timestamps, random IDs) so semantically identical plans always hash to the same value.

Approvals are bound to this hash (or its `plan_id` derivative). That means:

- An approval can **only** authorize the exact plan you previewed.
- If the plan changes, the hash changes, and the approval is rejected.
- The hash is the concrete proof that "what I approved" equals "what is about to run."

### Human CLI Flow

**Prepare**

```
wa prepare send --pane-id 3 "text"
wa prepare workflow run handle_compaction --pane-id 3
```

Output requirements:
- Show a **plan preview** (steps, preconditions, verification).
- Print `plan_id` and `plan_hash`.
- Print approval command: `wa approve <code>` (if approval required).
- Print commit command: `wa commit <plan_id>`.
- If approval not required, still require commit (to ensure the plan preview is the exact plan executed).

**Commit**

```
wa commit <plan_id>
```

Commit checks:
- Plan exists and is not expired.
- Approval exists and is **bound** to this plan hash (if required).
- Workspace matches.
- Target pane identity (pane_uuid) matches the plan (if applicable).
- TTL not expired.

### Robot Mode Flow

**Prepare**

```
wa robot prepare send --pane-id 3 "text"
```

Returns structured JSON:
- `plan_id`, `plan_hash`
- `plan` (full plan JSON)
- `requires_approval`
- `approval` (if required): `code`, `expires_at`, `command`
- `commit_command` (canonical)

**Commit**

```
wa robot commit <plan_id>
```

Returns:
- `status` (success/failed/denied)
- `execution_id` (if executed)
- `error_code` + `remediation` (if refused)

### MCP Flow

MCP mirrors Robot Mode:
- `wa.prepare` → same fields as `wa robot prepare`
- `wa.commit` → same fields as `wa robot commit`

### Binding Semantics (Required)

Approvals MUST bind to:
- `workspace_id`
- `plan_hash` (or derived `plan_id`)
- `action_kind(s)` present in the plan
- `pane_uuid` for any pane-scoped steps
- `ttl` (expires_at)

Commit MUST refuse execution if **any** binding check fails:
- Plan hash mismatch
- Approval expired or already consumed
- Workspace mismatch
- Pane identity mismatch (pane UUID changed)
- Plan no longer valid (preconditions fail)

### Error Codes + Remediation (Proposed)

| Error Code | Meaning | Remediation |
| --- | --- | --- |
| `E_PLAN_NOT_FOUND` | Plan ID missing | Re-run `wa prepare ...` |
| `E_PLAN_EXPIRED` | Plan TTL expired | Re-run prepare + approve |
| `E_PLAN_HASH_MISMATCH` | Approval bound to different plan hash | Re-run prepare + approve |
| `E_PLAN_APPROVAL_MISSING` | Approval required but missing | Run `wa approve <code>` |
| `E_PLAN_PANE_MISMATCH` | Pane identity changed | Re-run prepare for current pane |
| `E_PLAN_PRECONDITION_FAILED` | Plan preconditions no longer true | Re-run prepare |

This design ensures **explicit user intent**: the plan preview is exactly what gets executed, and approvals cannot be replayed against different actions.

### Troubleshooting Checklist

If prepare/commit fails, use this checklist before retrying:

1. **Plan expired** (`E_PLAN_EXPIRED`): plans have TTLs to prevent replay. Re-run `wa prepare ...` and approve again.
2. **Hash mismatch** (`E_PLAN_HASH_MISMATCH`): the plan changed. Re-run prepare and use the new plan hash/ID.
3. **Pane mismatch** (`E_PLAN_PANE_MISMATCH`): the pane identity changed (pane restarted, new session). Re-run prepare on the current pane.
4. **Precondition failed** (`E_PLAN_PRECONDITION_FAILED`): environment drift (alt-screen, prompt state, etc.). Resolve the precondition and re-run prepare.

If you keep hitting mismatches, capture the plan preview, compare it to the latest plan, and verify you are approving the most recent plan hash.

## Extension Guidance (Adding New Step Kinds Safely)

When introducing a new StepAction, Precondition, or Verification strategy:

1. **Define the enum variant** in `crates/wa-core/src/plan.rs` with stable serde tags.
2. **Implement canonical_string** for determinism. Avoid volatile fields (timestamps, random IDs).
3. **Add tests** that assert canonical stability and serialization round-trips.
4. **Wire policy/audit**:
   - Ensure actions that mutate panes are policy-gated.
   - Ensure any text payload is redacted in logs/audits (never store raw secrets).
5. **Update renderers**:
   - Human output (`wa` CLI) should show a clear summary line.
   - Robot/MCP output should include structured fields without secrets.
6. **Update JSON schemas** if ActionPlan shapes appear in robot/web outputs (e.g., `docs/json-schema/wa-robot-workflow-status.json`).
7. **Document behavior** here so future contributors understand when to use the new step.

If a step needs *conditional branching*, prefer representing it as multiple explicit steps plus a verification/abort strategy. Avoid implicit branching inside actions; it breaks determinism and preview accuracy.

### Contributor Checklist (File-by-File)

Use this checklist when adding a new `StepAction`/`Precondition`/`VerificationStrategy` variant so implementation stays deterministic and auditable:

1. **Schema + serde tags**
   - Edit `crates/wa-core/src/plan.rs`.
   - Add the new enum variant with stable `snake_case` serde tags and deterministic field ordering.
2. **Canonicalization + hashing**
   - Update canonical string helpers in `crates/wa-core/src/plan.rs` (`canonical_*` helpers).
   - Ensure no volatile values (timestamps, random IDs, non-deterministic map iteration) influence hashes.
3. **Execution wiring**
   - Wire execution semantics in `crates/wa-core/src/workflows.rs` (plan generation + runtime handling).
   - Ensure precondition and verification behavior is explicit (no hidden branching).
4. **Policy + redaction**
   - If the action can mutate panes or carry user text, verify policy gating and redaction paths in `crates/wa-core/src/policy.rs`, `crates/wa-core/src/approval.rs`, and storage/audit logging paths.
5. **Surface parity**
   - Verify dry-run/human CLI (`crates/wa/src/main.rs`), robot output, and MCP output expose the new variant consistently.
   - Update docs/schemas if output contracts change.
6. **Tests (required before merge)**
   - Add/extend tests in `crates/wa-core/src/plan.rs` for serialization and canonical hash stability.
   - Add/extend execution tests in `crates/wa-core/src/workflows.rs` or `crates/wa-core/tests/`.
   - Run:
     - `cargo test -p wa-core plan`
     - `cargo test -p wa-core workflows`
     - `cargo check --all-targets`

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

    /// Wait for pane output tail to be stable
    StableTail {
        #[serde(skip_serializing_if = "Option::is_none")]
        pane_id: Option<u64>,
        stable_for_ms: u64,
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
