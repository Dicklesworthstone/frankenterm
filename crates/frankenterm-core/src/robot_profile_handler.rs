//! Real handler for the `robot profile` family (ft-b0g7g).
//!
//! Replaces the `build_ntm_not_implemented_response` placeholder at
//! `crates/frankenterm/src/main.rs:23270`. The handler operates on a
//! [`StorageBackend`](crate::storage_backend_trait::StorageBackend) so
//! the conformance test in `tests/robot_family_conformance.rs` can drive
//! it against a fresh in-memory backend without standing up the full
//! async `StorageHandle`.
//!
//! ## Action coverage
//!
//! - `list` — reads `agent_profiles` via
//!   [`crate::storage::agent_profiles_sql::list_agent_profiles`],
//!   honours both `role_filter` (pushed down to SQL) and
//!   `tag_filter` (filtered in-process because tags are stored as a
//!   JSON array and the role index is the only one shipped). Returns
//!   `ProfileListData { profiles: Vec<ProfileSummary> }`.
//! - `show` — reads one row by name; returns `ProfileShowData` or a
//!   typed `NotFound` error.
//! - `validate` — looks the row up, runs
//!   [`AgentProfile::validate`], reports `valid` / `issues`.
//! - `apply` — verifies the row exists; for `dry_run = true` returns
//!   the planned `ProfileApplyData`; for `dry_run = false` this
//!   standalone handler reports a typed `SpawnFailed` because real
//!   spawning is daemon-mediated and must run through the mux service.
//!   The contract spec at
//!   `docs/robot-contracts/profile.md` calls `apply` `Sequential`
//!   (mutating, transactional); this slice is the read-only-safe
//!   subset of that contract.

use std::collections::{BTreeMap, HashMap};
use std::hash::BuildHasher;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

use crate::agent_profiles::{AgentProfile, ProfileValidationError};
use crate::fleet_mutation::{
    FleetMutationAction, FleetMutationExecutionError, FleetMutationExecutor, FleetMutationLedger,
    FleetMutationPlan, FleetMutationPlanError, FleetMutationPolicyDecision, FleetMutationReceipt,
    FleetMutationReceiptStatus, FleetMutationStep, FleetMutationStepOutput,
};

// br-ft-iwg7x: bootstrap-commands metadata can hold malformed JSON.
// `parse_bootstrap_commands` silently substituted Vec::new() on
// serde failure, indistinguishable from a profile that simply
// declared no bootstrap_commands. Operators saw "agent started but
// did not run my pre-flight setup" with no signal pointing at the
// dropped parse. This counter bumps on every malformed parse so a
// single observability scrape surfaces the misconfiguration.
//
// Same observability defect family as ft-zkthg
// (workflows_serde_drop_count), ft-jyywz (audit_chain_export_dropped_count),
// ft-yygus (policy_decision_context_serde_drop_count), ft-rnpuc
// (mcp_clock_anomaly_count), and ft-bn6qi (epoch_clock_anomaly_count).
static ROBOT_PROFILE_BOOTSTRAP_SERDE_DROP_COUNT: AtomicU64 = AtomicU64::new(0);

/// br-ft-iwg7x: cumulative count of `bootstrap_commands` metadata
/// fields dropped because the persisted JSON failed to parse as
/// `Vec<String>`. Each increment represents one profile-apply
/// where the operator's intended bootstrap commands were silently
/// replaced with the empty list. > 0 means investigate profile
/// metadata authoring (likely hand-edited JSON with a stray
/// comma, unquoted entry, or wrong-shape value).
#[must_use]
pub fn robot_profile_bootstrap_serde_drop_count() -> u64 {
    ROBOT_PROFILE_BOOTSTRAP_SERDE_DROP_COUNT.load(Ordering::Relaxed)
}

/// Test helper: reset the counter so regression tests can assert
/// post-bump values without state leakage between tests.
#[cfg(test)]
pub(crate) fn reset_robot_profile_bootstrap_serde_drop_count_for_test() {
    ROBOT_PROFILE_BOOTSTRAP_SERDE_DROP_COUNT.store(0, Ordering::Relaxed);
}

#[inline]
fn record_robot_profile_bootstrap_serde_drop() {
    ROBOT_PROFILE_BOOTSTRAP_SERDE_DROP_COUNT.fetch_add(1, Ordering::Relaxed);
}
use crate::robot_ntm_surface::{
    ProfileApplyData, ProfileListData, ProfileShowData, ProfileSummary, ProfileValidateData,
};
use crate::storage::agent_profiles_sql::{
    AgentProfileSqlError, get_agent_profile, list_agent_profiles,
};
use crate::storage::profiles_applied_log_sql::{
    ProfilesAppliedLogSqlError, get_apply_receipt, insert_apply_receipt,
};
use crate::storage_backend_trait::StorageBackend;

/// Failure modes the handler exposes to its caller. The wire-level
/// envelope at `main.rs` translates each variant into a typed
/// `RobotResponse` error code; the conformance test only sees the
/// success path because it pre-seeds a row named `default`.
#[derive(Debug)]
pub enum ProfileHandlerError {
    /// Action name is not one of `list` / `show` / `apply` /
    /// `validate`. Indicates a CLI-to-handler dispatch bug, not a
    /// caller error.
    UnknownAction(String),
    /// A required parameter is missing or the wrong shape (e.g.,
    /// `name` is not a string).
    BadParams(String),
    /// Profile lookup returned `None` for a name the action requires
    /// to exist.
    NotFound { name: String },
    /// Underlying SQL primitive failed (schema, deserialization,
    /// duplicate constraint, …).
    Storage(AgentProfileSqlError),
    /// `apply` with `dry_run = false` failed before pane spawning
    /// completed.
    SpawnFailed { reason: String },
    /// The stored profile or request-specific overrides failed
    /// validation before any daemon/mux mutation was attempted.
    ValidationFailed { name: String, issues: Vec<String> },
    /// Daemon-side policy rejected the mutation before side effects.
    PolicyDenied { reason: String },
    /// Daemon-side policy requires a human approval before side effects.
    ApprovalRequired { reason: String },
    /// The mutation substrate failed before or during execution.
    ApplyFailed { error_code: String, reason: String },
    /// A post-spawn failure required rollback and rollback did not
    /// complete cleanly.
    CompensationFailed { reason: String },
    /// The durable profile-apply receipt log failed.
    ApplyLog(ProfilesAppliedLogSqlError),
    /// The shared mutation plan substrate rejected the plan.
    MutationPlan(FleetMutationPlanError),
}

impl std::fmt::Display for ProfileHandlerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownAction(a) => write!(f, "unknown profile action `{a}`"),
            Self::BadParams(msg) => write!(f, "invalid profile request: {msg}"),
            Self::NotFound { name } => write!(f, "profile `{name}` not found"),
            Self::Storage(e) => write!(f, "profile storage error: {e}"),
            Self::SpawnFailed { reason } => write!(f, "profile apply spawn failed: {reason}"),
            Self::ValidationFailed { name, issues } => {
                write!(
                    f,
                    "profile `{name}` failed validation: {}",
                    issues.join("; ")
                )
            }
            Self::PolicyDenied { reason } => write!(f, "profile apply policy denied: {reason}"),
            Self::ApprovalRequired { reason } => {
                write!(f, "profile apply requires approval: {reason}")
            }
            Self::ApplyFailed { error_code, reason } => {
                write!(f, "profile apply failed ({error_code}): {reason}")
            }
            Self::CompensationFailed { reason } => {
                write!(f, "profile apply compensation failed: {reason}")
            }
            Self::ApplyLog(e) => write!(f, "profile apply receipt log error: {e}"),
            Self::MutationPlan(e) => write!(f, "profile apply mutation plan error: {e}"),
        }
    }
}

impl std::error::Error for ProfileHandlerError {}

impl ProfileHandlerError {
    /// Stable error code for the wire envelope. Matches the family of
    /// codes [`crate::robot_envelope`] uses for typed failures.
    #[must_use]
    pub fn error_code(&self) -> &str {
        match self {
            Self::UnknownAction(_) => "robot.profile.unknown_action",
            Self::BadParams(_) => "robot.profile.bad_params",
            Self::NotFound { .. } => "robot.profile.not_found",
            Self::Storage(_) => "robot.profile.storage",
            Self::SpawnFailed { .. } => "robot.profile.spawn_failed",
            Self::ValidationFailed { .. } => "robot.profile.validation_failed",
            Self::PolicyDenied { .. } => "robot.profile.policy_denied",
            Self::ApprovalRequired { .. } => "robot.profile.require_approval",
            Self::ApplyFailed { error_code, .. } => error_code.as_str(),
            Self::CompensationFailed { .. } => "robot.profile.compensation_failed",
            Self::ApplyLog(_) => "robot.profile.apply_log",
            Self::MutationPlan(_) => "robot.profile.mutation_plan",
        }
    }
}

impl From<ProfilesAppliedLogSqlError> for ProfileHandlerError {
    fn from(err: ProfilesAppliedLogSqlError) -> Self {
        Self::ApplyLog(err)
    }
}

impl From<FleetMutationPlanError> for ProfileHandlerError {
    fn from(err: FleetMutationPlanError) -> Self {
        Self::MutationPlan(err)
    }
}

/// Dispatch entry point. Returns the handler's success payload as a
/// JSON `Value` matching the per-action response schema declared in
/// [`crate::robot_family_contract::profile_family_contract`]. The
/// caller wraps the value (or the error) in a `RobotResponse`
/// envelope.
///
/// `params` is the canonical request object — the `params` of the
/// `RobotNtmCommand::Profile(ProfileCommand::*)` request after the
/// CLI has parsed its flags into JSON. Nested keys mirror the
/// `Profile*Request` types in [`crate::robot_ntm_surface`].
pub fn handle_profile_command(
    action: &str,
    params: &Value,
    backend: &dyn StorageBackend,
) -> Result<Value, ProfileHandlerError> {
    match action {
        "list" => handle_list(params, backend),
        "show" => handle_show(params, backend),
        "apply" => handle_apply(params, backend),
        "validate" => handle_validate(params, backend),
        other => Err(ProfileHandlerError::UnknownAction(other.to_string())),
    }
}

fn require_str<'a>(params: &'a Value, key: &str) -> Result<&'a str, ProfileHandlerError> {
    params
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| ProfileHandlerError::BadParams(format!("missing required field `{key}`")))
}

fn opt_str<'a>(params: &'a Value, key: &str) -> Option<&'a str> {
    params.get(key).and_then(Value::as_str)
}

fn opt_u32(params: &Value, key: &str) -> Option<u32> {
    params
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
}

fn opt_bool(params: &Value, key: &str) -> Option<bool> {
    params.get(key).and_then(Value::as_bool)
}

fn to_value<T: serde::Serialize>(v: &T) -> Result<Value, ProfileHandlerError> {
    serde_json::to_value(v).map_err(|e| ProfileHandlerError::BadParams(e.to_string()))
}

fn summarize(profile: AgentProfile) -> ProfileSummary {
    ProfileSummary {
        description: profile.metadata.get("description").cloned(),
        name: profile.name,
        role: profile.role,
        tags: profile.tags,
    }
}

fn handle_list(params: &Value, backend: &dyn StorageBackend) -> Result<Value, ProfileHandlerError> {
    let role_filter = opt_str(params, "role_filter");
    let tag_filter = opt_str(params, "tag_filter").map(str::to_string);
    let rows = list_agent_profiles(backend, role_filter).map_err(ProfileHandlerError::Storage)?;
    let profiles: Vec<ProfileSummary> = rows
        .into_iter()
        .filter(|row| match &tag_filter {
            None => true,
            Some(tag) => row.tags.iter().any(|t| t == tag),
        })
        .map(summarize)
        .collect();
    to_value(&ProfileListData { profiles })
}

fn handle_show(params: &Value, backend: &dyn StorageBackend) -> Result<Value, ProfileHandlerError> {
    let name = require_str(params, "name")?;
    let profile = get_agent_profile(backend, name)
        .map_err(ProfileHandlerError::Storage)?
        .ok_or_else(|| ProfileHandlerError::NotFound {
            name: name.to_string(),
        })?;
    let data = ProfileShowData {
        description: profile.metadata.get("description").cloned(),
        working_directory: profile.metadata.get("working_directory").cloned(),
        layout_template: profile.metadata.get("layout_template").cloned(),
        bootstrap_commands: parse_bootstrap_commands(&profile.metadata),
        name: profile.name,
        role: profile.role,
        spawn_command: profile.command,
        environment: profile.env,
        tags: profile.tags,
    };
    // The contract's response_schema (additionalProperties: false,
    // single-type per field) cannot encode `string | null`, so strip
    // null entries from the serialized response. Required fields are
    // never `Option<_>` in `ProfileShowData`, so this preserves the
    // contract's `required` invariant.
    Ok(strip_null_fields(to_value(&data)?))
}

fn handle_validate(
    params: &Value,
    backend: &dyn StorageBackend,
) -> Result<Value, ProfileHandlerError> {
    let name = require_str(params, "name")?;
    let row = get_agent_profile(backend, name).map_err(ProfileHandlerError::Storage)?;
    let (valid, issues) = match row {
        None => (
            false,
            vec![format!("profile `{name}` not found in agent_profiles")],
        ),
        Some(profile) => match profile.validate() {
            Ok(()) => (true, Vec::new()),
            Err(err) => (false, vec![format_validation_error(&err)]),
        },
    };
    to_value(&ProfileValidateData {
        name: name.to_string(),
        valid,
        issues,
    })
}

fn handle_apply(
    params: &Value,
    backend: &dyn StorageBackend,
) -> Result<Value, ProfileHandlerError> {
    let name = require_str(params, "name")?;
    let count = opt_u32(params, "count").unwrap_or(1);
    let dry_run = opt_bool(params, "dry_run").unwrap_or(false);

    // Verify the row exists before reporting any "would spawn" plan.
    let _profile = get_agent_profile(backend, name)
        .map_err(ProfileHandlerError::Storage)?
        .ok_or_else(|| ProfileHandlerError::NotFound {
            name: name.to_string(),
        })?;

    if dry_run {
        return to_value(&ProfileApplyData {
            profile_name: name.to_string(),
            // Plan only — actual pane IDs come from the daemon-side
            // spawn loop. Surface count via the response shape: an
            // empty `panes_spawned` array combined with `dry_run:
            // true` is the contract's "no mutation" signal.
            panes_spawned: Vec::new(),
            dry_run: true,
        });
    }

    // Real spawn requires the daemon-side mux machinery. The
    // standalone handler cannot perform that mutation directly.
    Err(ProfileHandlerError::SpawnFailed {
        reason: format!(
            "profile `{name}` apply (count={count}) requires daemon-mediated pane spawning; \
             daemon apply RPC is not connected in this process"
        ),
    })
}

/// Concrete spawn request derived from one profile-apply step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileApplySpawnSpec {
    pub step_id: String,
    pub profile_name: String,
    pub ordinal: u32,
    pub shell: String,
    pub command: Option<String>,
    pub working_directory: Option<String>,
    pub domain: Option<String>,
    pub env: BTreeMap<String, String>,
    pub bootstrap_commands: Vec<String>,
}

/// Failure from the daemon/mux adapter while applying a profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileApplyExecutionError {
    pub error_code: String,
    pub message: String,
}

impl ProfileApplyExecutionError {
    #[must_use]
    pub fn new(error_code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            error_code: error_code.into(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ProfileApplyExecutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.error_code, self.message)
    }
}

impl std::error::Error for ProfileApplyExecutionError {}

/// Daemon/mux adapter used by live profile apply.
///
/// The production CLI adapter spawns panes through the mux bridge. Tests supply
/// fakes so policy denial, idempotent replay, partial failure, and rollback
/// semantics can be exercised without touching a real terminal session.
pub trait ProfileApplyMutationExecutor {
    fn policy_for_spawn(&self, _spec: &ProfileApplySpawnSpec) -> FleetMutationPolicyDecision {
        FleetMutationPolicyDecision::Allow
    }

    fn spawn_agent(
        &mut self,
        spec: &ProfileApplySpawnSpec,
    ) -> Result<u64, ProfileApplyExecutionError>;

    fn stop_agent(&mut self, pane_id: u64, reason: &str) -> Result<(), ProfileApplyExecutionError>;
}

struct ProfileFleetMutationExecutor<'a, E: ProfileApplyMutationExecutor> {
    executor: &'a mut E,
    specs: BTreeMap<String, ProfileApplySpawnSpec>,
    spawned_by_step: BTreeMap<String, u64>,
}

impl<'a, E: ProfileApplyMutationExecutor> ProfileFleetMutationExecutor<'a, E> {
    fn new(executor: &'a mut E, specs: &[ProfileApplySpawnSpec]) -> Self {
        Self {
            executor,
            specs: specs
                .iter()
                .map(|spec| (spec.step_id.clone(), spec.clone()))
                .collect(),
            spawned_by_step: BTreeMap::new(),
        }
    }

    fn compensate_spawned_after_log_failure(&mut self) -> Result<(), ProfileApplyExecutionError> {
        let spawned: Vec<(String, u64)> = self
            .spawned_by_step
            .iter()
            .map(|(step_id, pane_id)| (step_id.clone(), *pane_id))
            .collect();
        for (step_id, pane_id) in spawned.into_iter().rev() {
            self.executor.stop_agent(
                pane_id,
                "profile apply receipt logging failed; rolling back spawned pane",
            )?;
            self.spawned_by_step.remove(&step_id);
        }
        Ok(())
    }
}

impl<E: ProfileApplyMutationExecutor> FleetMutationExecutor
    for ProfileFleetMutationExecutor<'_, E>
{
    fn execute_step(
        &mut self,
        step: &FleetMutationStep,
    ) -> Result<FleetMutationStepOutput, FleetMutationExecutionError> {
        match &step.action {
            FleetMutationAction::SpawnAgent { .. } => {
                let spec = self.specs.get(&step.step_id).ok_or_else(|| {
                    FleetMutationExecutionError::new(
                        "robot.profile.internal_missing_spawn_spec",
                        format!("missing spawn spec for step {}", step.step_id),
                    )
                })?;
                let pane_id = self
                    .executor
                    .spawn_agent(spec)
                    .map_err(profile_execution_to_fleet_error)?;
                self.spawned_by_step.insert(step.step_id.clone(), pane_id);
                let mut output = FleetMutationStepOutput {
                    pane_id: Some(pane_id),
                    ..FleetMutationStepOutput::default()
                };
                output
                    .details
                    .insert("profile_name".to_string(), spec.profile_name.clone());
                output
                    .details
                    .insert("ordinal".to_string(), spec.ordinal.to_string());
                if let Some(command) = &spec.command {
                    output
                        .details
                        .insert("command".to_string(), command.clone());
                }
                if let Some(cwd) = &spec.working_directory {
                    output.details.insert("cwd".to_string(), cwd.clone());
                }
                output.details.insert(
                    "bootstrap_command_count".to_string(),
                    spec.bootstrap_commands.len().to_string(),
                );
                Ok(output)
            }
            FleetMutationAction::StopAgent { pane_id, reason } => {
                self.executor
                    .stop_agent(*pane_id, reason)
                    .map_err(profile_execution_to_fleet_error)?;
                Ok(FleetMutationStepOutput::default())
            }
            FleetMutationAction::ReassignWork { .. } => Err(FleetMutationExecutionError::new(
                "robot.profile.unsupported_mutation_action",
                "profile apply executor cannot reassign work",
            )),
        }
    }

    fn compensate_step(
        &mut self,
        original: &FleetMutationStep,
        compensation: &FleetMutationAction,
    ) -> Result<FleetMutationStepOutput, FleetMutationExecutionError> {
        let pane_id = self
            .spawned_by_step
            .get(&original.step_id)
            .copied()
            .or(match compensation {
                FleetMutationAction::StopAgent { pane_id, .. } if *pane_id != 0 => Some(*pane_id),
                _ => None,
            })
            .ok_or_else(|| {
                FleetMutationExecutionError::new(
                    "robot.profile.compensation_missing_pane",
                    format!("no spawned pane recorded for step {}", original.step_id),
                )
            })?;
        let reason = match compensation {
            FleetMutationAction::StopAgent { reason, .. } => reason.as_str(),
            _ => "profile apply compensation",
        };
        self.executor
            .stop_agent(pane_id, reason)
            .map_err(profile_execution_to_fleet_error)?;
        self.spawned_by_step.remove(&original.step_id);
        let mut output = FleetMutationStepOutput {
            pane_id: Some(pane_id),
            ..FleetMutationStepOutput::default()
        };
        output
            .details
            .insert("compensated_step".to_string(), original.step_id.clone());
        Ok(output)
    }
}

fn profile_execution_to_fleet_error(
    err: ProfileApplyExecutionError,
) -> FleetMutationExecutionError {
    FleetMutationExecutionError::new(err.error_code, err.message)
}

/// Live profile apply entry point for daemon/mux-aware callers.
///
/// `handle_profile_command` intentionally remains the storage-only handler for
/// read paths and conformance harnesses. This function is the wired mutation
/// path: it validates the stored profile, checks the durable apply log for
/// idempotent replay, executes through the shared fleet mutation substrate, and
/// writes the apply receipt only after all requested panes have spawned.
pub fn handle_profile_apply_with_executor<E: ProfileApplyMutationExecutor>(
    params: &Value,
    backend: &dyn StorageBackend,
    executor: &mut E,
    now_ms: i64,
) -> Result<Value, ProfileHandlerError> {
    let name = require_str(params, "name")?;
    let count = opt_u32(params, "count").unwrap_or(1);
    let dry_run = opt_bool(params, "dry_run").unwrap_or(false);
    let env_overrides = parse_env_overrides(params)?;

    if count == 0 {
        return Err(ProfileHandlerError::BadParams(
            "`count` must be greater than zero".to_string(),
        ));
    }

    let profile = get_agent_profile(backend, name)
        .map_err(ProfileHandlerError::Storage)?
        .ok_or_else(|| ProfileHandlerError::NotFound {
            name: name.to_string(),
        })?;

    if let Err(err) = profile.validate() {
        return Err(ProfileHandlerError::ValidationFailed {
            name: name.to_string(),
            issues: vec![format_validation_error(&err)],
        });
    }

    let content_hash =
        compute_apply_content_hash(&profile.name, profile.updated_at_ms, count, &env_overrides);
    if !dry_run {
        if let Some(existing) = get_apply_receipt(backend, &content_hash)? {
            return profile_apply_replay_value(&existing);
        }
    }

    let specs = profile_apply_spawn_specs(&profile, count, &env_overrides);
    let plan = profile_apply_plan(&content_hash, dry_run, &specs, executor);
    let mut ledger = FleetMutationLedger::new();
    let mut fleet_executor = ProfileFleetMutationExecutor::new(executor, &specs);
    let receipt = ledger.execute_plan(&plan, &mut fleet_executor)?;

    match receipt.status {
        FleetMutationReceiptStatus::DryRun => {
            profile_apply_receipt_value(&profile.name, &content_hash, &receipt, Vec::new(), false)
        }
        FleetMutationReceiptStatus::Denied => Err(ProfileHandlerError::PolicyDenied {
            reason: first_blocking_policy_reason(&receipt)
                .unwrap_or_else(|| "profile apply denied by policy".to_string()),
        }),
        FleetMutationReceiptStatus::ApprovalRequired => {
            Err(ProfileHandlerError::ApprovalRequired {
                reason: first_blocking_policy_reason(&receipt)
                    .unwrap_or_else(|| "profile apply requires approval".to_string()),
            })
        }
        FleetMutationReceiptStatus::Succeeded => {
            let panes_spawned = panes_from_receipt(&receipt);
            let apply_receipt = ApplyReceipt {
                content_hash: content_hash.clone(),
                profile_name: profile.name.clone(),
                profile_updated_at_ms: profile.updated_at_ms,
                count,
                panes_spawned: panes_spawned.clone(),
                recorded_at_ms: now_ms,
            };
            match insert_apply_receipt(backend, &apply_receipt) {
                Ok(()) => profile_apply_receipt_value(
                    &profile.name,
                    &content_hash,
                    &receipt,
                    panes_spawned,
                    false,
                ),
                Err(ProfilesAppliedLogSqlError::Backend(_)) => {
                    if let Some(existing) = get_apply_receipt(backend, &content_hash)? {
                        fleet_executor
                            .compensate_spawned_after_log_failure()
                            .map_err(|err| ProfileHandlerError::CompensationFailed {
                                reason: err.to_string(),
                            })?;
                        profile_apply_replay_value(&existing)
                    } else {
                        fleet_executor
                            .compensate_spawned_after_log_failure()
                            .map_err(|err| ProfileHandlerError::CompensationFailed {
                                reason: err.to_string(),
                            })?;
                        Err(ProfileHandlerError::ApplyFailed {
                            error_code: "robot.profile.apply_log".to_string(),
                            reason: "profile apply spawned panes but durable receipt insert failed"
                                .to_string(),
                        })
                    }
                }
                Err(err) => {
                    fleet_executor
                        .compensate_spawned_after_log_failure()
                        .map_err(|err| ProfileHandlerError::CompensationFailed {
                            reason: err.to_string(),
                        })?;
                    Err(ProfileHandlerError::ApplyLog(err))
                }
            }
        }
        FleetMutationReceiptStatus::Failed | FleetMutationReceiptStatus::Compensated => {
            Err(ProfileHandlerError::ApplyFailed {
                error_code: first_step_error_code(&receipt)
                    .unwrap_or("robot.profile.spawn_failed")
                    .to_string(),
                reason: first_step_error_message(&receipt)
                    .unwrap_or_else(|| "profile apply failed before all panes spawned".to_string()),
            })
        }
        FleetMutationReceiptStatus::CompensationFailed => {
            Err(ProfileHandlerError::CompensationFailed {
                reason: first_step_error_message(&receipt).unwrap_or_else(|| {
                    "profile apply failed and rollback compensation failed".to_string()
                }),
            })
        }
    }
}

fn parse_env_overrides(params: &Value) -> Result<HashMap<String, String>, ProfileHandlerError> {
    let Some(value) = params.get("env_overrides") else {
        return Ok(HashMap::new());
    };
    let object = value.as_object().ok_or_else(|| {
        ProfileHandlerError::BadParams("`env_overrides` must be an object".to_string())
    })?;
    let mut env = HashMap::with_capacity(object.len());
    for (key, value) in object {
        let value = value.as_str().ok_or_else(|| {
            ProfileHandlerError::BadParams(format!("`env_overrides.{key}` must be a string value"))
        })?;
        env.insert(key.clone(), value.to_string());
    }
    Ok(env)
}

fn profile_apply_spawn_specs<S: BuildHasher>(
    profile: &AgentProfile,
    count: u32,
    env_overrides: &HashMap<String, String, S>,
) -> Vec<ProfileApplySpawnSpec> {
    let mut env: BTreeMap<String, String> = profile
        .env
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    for (key, value) in env_overrides {
        env.insert(key.clone(), value.clone());
    }
    let bootstrap_commands = parse_bootstrap_commands(&profile.metadata);
    let working_directory = profile.metadata.get("working_directory").cloned();
    let domain = profile.metadata.get("domain").cloned();

    (0..count)
        .map(|index| ProfileApplySpawnSpec {
            step_id: format!("spawn-{}", index + 1),
            profile_name: profile.name.clone(),
            ordinal: index + 1,
            shell: profile.shell.clone(),
            command: profile.command.clone(),
            working_directory: working_directory.clone(),
            domain: domain.clone(),
            env: env.clone(),
            bootstrap_commands: bootstrap_commands.clone(),
        })
        .collect()
}

fn profile_apply_plan<E: ProfileApplyMutationExecutor>(
    content_hash: &str,
    dry_run: bool,
    specs: &[ProfileApplySpawnSpec],
    executor: &E,
) -> FleetMutationPlan {
    let steps = specs
        .iter()
        .map(|spec| {
            let program = spec
                .command
                .clone()
                .filter(|command| !command.trim().is_empty())
                .unwrap_or_else(|| {
                    if spec.shell.trim().is_empty() {
                        "default-shell".to_string()
                    } else {
                        spec.shell.clone()
                    }
                });
            FleetMutationStep {
                step_id: spec.step_id.clone(),
                action: FleetMutationAction::SpawnAgent {
                    profile_name: spec.profile_name.clone(),
                    program,
                    cwd: spec.working_directory.clone(),
                    domain: spec.domain.clone(),
                    env: spec.env.clone(),
                },
                policy: executor.policy_for_spawn(spec),
                compensation: Some(FleetMutationAction::StopAgent {
                    pane_id: 0,
                    reason: format!(
                        "rollback profile `{}` apply step {}",
                        spec.profile_name, spec.ordinal
                    ),
                }),
            }
        })
        .collect();
    FleetMutationPlan::with_client_key(
        format!("profile-apply:{content_hash}"),
        dry_run,
        steps,
        content_hash,
    )
}

fn panes_from_receipt(receipt: &FleetMutationReceipt) -> Vec<u64> {
    receipt
        .steps
        .iter()
        .filter_map(|step| step.output.as_ref().and_then(|output| output.pane_id))
        .collect()
}

fn first_blocking_policy_reason(receipt: &FleetMutationReceipt) -> Option<String> {
    receipt.steps.iter().find_map(|step| match &step.policy {
        FleetMutationPolicyDecision::Allow => None,
        FleetMutationPolicyDecision::Deny { message, .. }
        | FleetMutationPolicyDecision::RequireApproval { message, .. } => Some(message.clone()),
    })
}

fn first_step_error_code(receipt: &FleetMutationReceipt) -> Option<&str> {
    receipt.steps.iter().find_map(|step| {
        step.error
            .as_ref()
            .map(|error| error.error_code.as_str())
            .or_else(|| {
                step.compensation
                    .as_ref()
                    .and_then(|comp| comp.error.as_ref())
                    .map(|error| error.error_code.as_str())
            })
    })
}

fn first_step_error_message(receipt: &FleetMutationReceipt) -> Option<String> {
    receipt.steps.iter().find_map(|step| {
        step.error
            .as_ref()
            .map(|error| error.message.clone())
            .or_else(|| {
                step.compensation
                    .as_ref()
                    .and_then(|comp| comp.error.as_ref())
                    .map(|error| error.message.clone())
            })
    })
}

fn profile_apply_receipt_value(
    profile_name: &str,
    content_hash: &str,
    receipt: &FleetMutationReceipt,
    panes_spawned: Vec<u64>,
    idempotent_replay: bool,
) -> Result<Value, ProfileHandlerError> {
    to_value(&serde_json::json!({
        "profile_name": profile_name,
        "panes_spawned": panes_spawned,
        "dry_run": receipt.dry_run,
        "requested_agents": receipt.steps.len(),
        "spawned_agents": receipt.completed_count,
        "skipped_existing_agents": if idempotent_replay { receipt.steps.len() } else { 0 },
        "policy_decisions": receipt.steps.iter().map(|step| {
            serde_json::json!({
                "step_id": step.step_id,
                "target_id": step.target_id,
                "policy": step.policy,
            })
        }).collect::<Vec<_>>(),
        "idempotency_key": content_hash,
        "mutation_idempotency_key": receipt.idempotency_key.as_str(),
        "rollback_status": mutation_status_label(receipt.status),
        "idempotent_replay": idempotent_replay || receipt.idempotent_replay,
        "mutation_receipt": receipt,
    }))
}

fn mutation_status_label(status: FleetMutationReceiptStatus) -> &'static str {
    match status {
        FleetMutationReceiptStatus::DryRun => "dry_run",
        FleetMutationReceiptStatus::Succeeded => "succeeded",
        FleetMutationReceiptStatus::Denied => "denied",
        FleetMutationReceiptStatus::ApprovalRequired => "approval_required",
        FleetMutationReceiptStatus::Failed => "failed",
        FleetMutationReceiptStatus::Compensated => "compensated",
        FleetMutationReceiptStatus::CompensationFailed => "compensation_failed",
    }
}

fn profile_apply_replay_value(receipt: &ApplyReceipt) -> Result<Value, ProfileHandlerError> {
    to_value(&serde_json::json!({
        "profile_name": receipt.profile_name,
        "panes_spawned": receipt.panes_spawned,
        "dry_run": false,
        "requested_agents": receipt.count,
        "spawned_agents": receipt.panes_spawned.len(),
        "skipped_existing_agents": receipt.panes_spawned.len(),
        "policy_decisions": [],
        "idempotency_key": receipt.content_hash,
        "mutation_idempotency_key": null,
        "rollback_status": "idempotent_replay",
        "idempotent_replay": true,
        "recorded_at_ms": receipt.recorded_at_ms,
    }))
}

/// Remove top-level fields whose serialized value is JSON `null`.
/// The contract's `additionalProperties: false` schema cannot
/// encode `string | null`, so emitting an absent key (rather than a
/// `null` entry) is the only shape that validates while still
/// honouring `required` on the non-optional fields.
fn strip_null_fields(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            Value::Object(map.into_iter().filter(|(_, v)| !v.is_null()).collect())
        }
        other => other,
    }
}

fn parse_bootstrap_commands(metadata: &HashMap<String, String>) -> Vec<String> {
    let Some(raw) = metadata.get("bootstrap_commands") else {
        return Vec::new();
    };
    match serde_json::from_str::<Vec<String>>(raw) {
        Ok(commands) => commands,
        Err(err) => {
            // br-ft-iwg7x: bump audit-fidelity counter and emit a
            // structured warn so operators can spot the silent drop
            // via metrics scrape AND log search.
            record_robot_profile_bootstrap_serde_drop();
            tracing::warn!(
                target: "frankenterm::robot_profile_handler",
                event = "br-ft-iwg7x",
                error = %err,
                raw_len = raw.len(),
                "robot profile bootstrap_commands metadata failed to parse as Vec<String>; \
                 falling back to empty list"
            );
            Vec::new()
        }
    }
}

fn format_validation_error(err: &ProfileValidationError) -> String {
    format!("{err:?}")
}

// ============================================================================
// br-ft-4iz0q substrate-pass: ApplyReceipt content-hash for the
// idempotency contract. The wired-pass cont-bead writes these into a
// new `profiles_applied_log` table; this module ships the pure-data
// type + the canonical-hash helper so the daemon-side handler can
// drop into a finished substrate.
// ============================================================================

/// Idempotency receipt for a non-dry-run `RobotProfile.apply` call.
///
/// br-ft-4iz0q (continuation of br-ft-b0g7g.cont.apply_spawn) names
/// the bead's idempotency rule:
///
/// > Compute an `ApplyReceipt` content-hash from `(profile_name,
/// > profile.updated_at_ms, env_overrides_canonicalized, count)` and
/// > reject duplicate apply requests with `ProfileApplyData {
/// > panes_spawned: <prior_apply>, dry_run: false }`.
///
/// The substrate ships the hash + the receipt payload type; the
/// wired-pass writes them into the `profiles_applied_log` table and
/// the daemon-side handler consults the log before issuing real mux
/// spawn calls.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ApplyReceipt {
    /// Hex SHA-256 of the canonical
    /// `(profile_name, updated_at_ms, env_overrides, count)` tuple.
    /// Stable across runs of the same logical apply.
    pub content_hash: String,
    /// Profile name the apply targeted. Carried for log readability;
    /// the hash already binds it.
    pub profile_name: String,
    /// `updated_at_ms` of the profile row at apply time. Bumping the
    /// profile in storage invalidates earlier receipts (the hash
    /// changes), so an apply against the new revision is NOT
    /// idempotent against the old one — that's the bead's intent.
    pub profile_updated_at_ms: i64,
    /// Number of panes the apply requested. Re-applies must specify
    /// the same count to hit the receipt.
    pub count: u32,
    /// Pane IDs the original apply spawned. Returned in the dedup
    /// path's `ProfileApplyData.panes_spawned` so callers see the
    /// same list they would have seen on first success.
    pub panes_spawned: Vec<u64>,
    /// Unix epoch ms when the receipt was first written.
    pub recorded_at_ms: i64,
}

/// Compute the canonical content hash for an apply request.
///
/// Inputs are canonicalised before hashing so the same logical apply
/// produces a stable hash regardless of HashMap iteration order:
///
/// 1. A version tag is written first so future encodings cannot
///    collide with this one.
/// 2. String and byte-vector fields are length-prefixed before their
///    bytes are written.
/// 3. `env_overrides` entries are sorted by key and encoded as
///    length-prefixed key/value pairs.
/// 4. SHA-256 over the pre-image, hex-encoded lowercase.
///
/// The length prefixes are required because profile env overrides may
/// contain separator-like bytes such as `=` or NUL; delimiter-based
/// encodings make `{A: "B\0C=D"}` collide with `{A: "B", C: "D"}`.
///
/// The pre-image format is **not** stable across major releases —
/// receipts written under one version may not match receipts written
/// under another. The wired-pass migration handles old-format rows
/// by dropping them (re-applies just pay the spawn cost once more).
#[must_use]
pub fn compute_apply_content_hash<S: BuildHasher>(
    profile_name: &str,
    profile_updated_at_ms: i64,
    count: u32,
    env_overrides: &HashMap<String, String, S>,
) -> String {
    use sha2::{Digest as _, Sha256};

    fn hash_len_prefixed(hasher: &mut Sha256, bytes: &[u8]) {
        let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        hasher.update(len.to_be_bytes());
        hasher.update(bytes);
    }

    let mut hasher = Sha256::new();
    hasher.update(b"robot_profile_apply_hash_v2");
    hash_len_prefixed(&mut hasher, profile_name.as_bytes());
    hasher.update(profile_updated_at_ms.to_be_bytes());
    hasher.update(count.to_be_bytes());

    // Sort env_overrides by key for deterministic hashing.
    let mut env_sorted: Vec<(&String, &String)> = env_overrides.iter().collect();
    env_sorted.sort_by(|a, b| a.0.cmp(b.0));
    let env_len = u64::try_from(env_sorted.len()).unwrap_or(u64::MAX);
    hasher.update(env_len.to_be_bytes());
    for (k, v) in env_sorted {
        hash_len_prefixed(&mut hasher, k.as_bytes());
        hash_len_prefixed(&mut hasher, v.as_bytes());
    }

    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod apply_receipt_tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn hash_is_stable_for_same_inputs() {
        let env: HashMap<String, String> =
            std::iter::once(("FOO".to_string(), "bar".to_string())).collect();
        let h1 = compute_apply_content_hash("dev", 1_700_000_000_000, 3, &env);
        let h2 = compute_apply_content_hash("dev", 1_700_000_000_000, 3, &env);
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64);
        assert!(h1.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn hash_diverges_when_profile_name_changes() {
        let env = HashMap::new();
        let a = compute_apply_content_hash("dev", 0, 1, &env);
        let b = compute_apply_content_hash("prod", 0, 1, &env);
        assert_ne!(a, b);
    }

    #[test]
    fn hash_diverges_when_updated_at_ms_changes() {
        let env = HashMap::new();
        let a = compute_apply_content_hash("dev", 1, 1, &env);
        let b = compute_apply_content_hash("dev", 2, 1, &env);
        assert_ne!(a, b);
    }

    #[test]
    fn hash_diverges_when_count_changes() {
        let env = HashMap::new();
        let a = compute_apply_content_hash("dev", 0, 3, &env);
        let b = compute_apply_content_hash("dev", 0, 4, &env);
        assert_ne!(a, b);
    }

    #[test]
    fn hash_canonicalises_env_overrides_iteration_order() {
        // Two HashMaps with identical entries should hash identically
        // even if their internal iteration order differs (the hasher
        // sorts before consuming).
        let mut env_a = HashMap::new();
        env_a.insert("FOO".to_string(), "1".to_string());
        env_a.insert("BAR".to_string(), "2".to_string());
        env_a.insert("BAZ".to_string(), "3".to_string());

        let mut env_b = HashMap::new();
        env_b.insert("BAZ".to_string(), "3".to_string());
        env_b.insert("BAR".to_string(), "2".to_string());
        env_b.insert("FOO".to_string(), "1".to_string());

        assert_eq!(
            compute_apply_content_hash("p", 0, 1, &env_a),
            compute_apply_content_hash("p", 0, 1, &env_b),
        );
    }

    #[test]
    fn hash_diverges_when_env_value_changes() {
        let mut env_a = HashMap::new();
        env_a.insert("FOO".to_string(), "1".to_string());
        let mut env_b = HashMap::new();
        env_b.insert("FOO".to_string(), "2".to_string());
        assert_ne!(
            compute_apply_content_hash("p", 0, 1, &env_a),
            compute_apply_content_hash("p", 0, 1, &env_b),
        );
    }

    #[test]
    fn hash_no_collision_between_separator_chars_in_value() {
        // Defensive: the previous delimiter-based pre-image made
        // these two distinct maps encode to the same bytes:
        // "A=B\0C=D\0".
        let mut env_a = HashMap::new();
        env_a.insert("A".to_string(), "B\0C=D".to_string());

        let mut env_b = HashMap::new();
        env_b.insert("A".to_string(), "B".to_string());
        env_b.insert("C".to_string(), "D".to_string());

        assert_ne!(
            compute_apply_content_hash("p", 0, 1, &env_a),
            compute_apply_content_hash("p", 0, 1, &env_b),
        );
    }

    #[test]
    fn apply_receipt_serde_roundtrip() {
        let receipt = ApplyReceipt {
            content_hash: "abc123".to_string(),
            profile_name: "dev".to_string(),
            profile_updated_at_ms: 1_700_000_000_000,
            count: 3,
            panes_spawned: vec![10, 11, 12],
            recorded_at_ms: 1_700_000_001_000,
        };
        let serialized = serde_json::to_string(&receipt).unwrap();
        let parsed: ApplyReceipt = serde_json::from_str(&serialized).unwrap();
        assert_eq!(parsed, receipt);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::agent_profiles_sql::insert_agent_profile;
    use crate::storage_backend_trait::{OpenConfig, RusqliteBackend, StorageBackend};
    use serde_json::json;

    fn fresh_conn() -> RusqliteBackend {
        let backend = RusqliteBackend::open(
            ":memory:",
            &OpenConfig {
                wal_mode: false,
                ..OpenConfig::default()
            },
        )
        .expect("open in-memory");
        backend
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS agent_profiles (
                name           TEXT PRIMARY KEY NOT NULL,
                role           TEXT NOT NULL DEFAULT '',
                tags           TEXT NOT NULL DEFAULT '[]',
                shell          TEXT NOT NULL DEFAULT '',
                command        TEXT,
                env            TEXT NOT NULL DEFAULT '{}',
                metadata       TEXT NOT NULL DEFAULT '{}',
                created_at_ms  INTEGER NOT NULL,
                updated_at_ms  INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS agent_profiles_role_idx
                ON agent_profiles(role);",
            )
            .expect("schema");
        backend
    }

    fn synth(name: &str, role: &str, tags: Vec<&str>) -> AgentProfile {
        let mut metadata = HashMap::new();
        metadata.insert("description".to_string(), format!("synth {name}"));
        metadata.insert("working_directory".to_string(), "/repo".to_string());
        AgentProfile {
            name: name.to_string(),
            role: role.to_string(),
            tags: tags.into_iter().map(str::to_string).collect(),
            shell: "/bin/sh".to_string(),
            command: Some("echo".to_string()),
            env: HashMap::new(),
            metadata,
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    fn fresh_conn_with_apply_log() -> RusqliteBackend {
        let backend = fresh_conn();
        backend
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS profiles_applied_log (
                content_hash          TEXT PRIMARY KEY NOT NULL,
                profile_name          TEXT NOT NULL,
                profile_updated_at_ms INTEGER NOT NULL,
                count                 INTEGER NOT NULL,
                panes_spawned_json    TEXT NOT NULL DEFAULT '[]',
                recorded_at_ms        INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS profiles_applied_log_profile_name_idx
                ON profiles_applied_log(profile_name);",
            )
            .expect("apply log schema");
        backend
    }

    #[derive(Default)]
    struct FakeApplyExecutor {
        spawned: Vec<ProfileApplySpawnSpec>,
        stopped: Vec<(u64, String)>,
        next_pane_id: u64,
        fail_on_ordinal: Option<u32>,
        fail_stop: bool,
        policy: Option<FleetMutationPolicyDecision>,
    }

    impl FakeApplyExecutor {
        fn new() -> Self {
            Self {
                next_pane_id: 100,
                ..Self::default()
            }
        }
    }

    impl ProfileApplyMutationExecutor for FakeApplyExecutor {
        fn policy_for_spawn(&self, _spec: &ProfileApplySpawnSpec) -> FleetMutationPolicyDecision {
            self.policy
                .clone()
                .unwrap_or(FleetMutationPolicyDecision::Allow)
        }

        fn spawn_agent(
            &mut self,
            spec: &ProfileApplySpawnSpec,
        ) -> Result<u64, ProfileApplyExecutionError> {
            self.spawned.push(spec.clone());
            if self.fail_on_ordinal == Some(spec.ordinal) {
                return Err(ProfileApplyExecutionError::new(
                    "robot.profile.mux_spawn_failed",
                    format!("spawn {} failed", spec.ordinal),
                ));
            }
            let pane_id = self.next_pane_id;
            self.next_pane_id += 1;
            Ok(pane_id)
        }

        fn stop_agent(
            &mut self,
            pane_id: u64,
            reason: &str,
        ) -> Result<(), ProfileApplyExecutionError> {
            self.stopped.push((pane_id, reason.to_string()));
            if self.fail_stop {
                return Err(ProfileApplyExecutionError::new(
                    "robot.profile.mux_compensation_failed",
                    format!("failed to stop pane {pane_id}"),
                ));
            }
            Ok(())
        }
    }

    #[test]
    fn list_returns_empty_when_table_empty() {
        let conn = fresh_conn();
        let v = handle_profile_command("list", &json!({}), &conn).expect("ok");
        let arr = v.get("profiles").and_then(Value::as_array).expect("arr");
        assert!(arr.is_empty());
    }

    #[test]
    fn list_returns_summaries_in_name_order() {
        let conn = fresh_conn();
        insert_agent_profile(&conn, &synth("z", "alpha", vec![])).unwrap();
        insert_agent_profile(&conn, &synth("a", "alpha", vec![])).unwrap();
        let v = handle_profile_command("list", &json!({}), &conn).expect("ok");
        let arr = v.get("profiles").and_then(Value::as_array).unwrap();
        let names: Vec<&str> = arr
            .iter()
            .map(|p| p.get("name").and_then(Value::as_str).unwrap())
            .collect();
        assert_eq!(names, vec!["a", "z"]);
    }

    #[test]
    fn list_role_filter_pushes_down_to_sql() {
        let conn = fresh_conn();
        insert_agent_profile(&conn, &synth("alpha-1", "alpha", vec![])).unwrap();
        insert_agent_profile(&conn, &synth("beta-1", "beta", vec![])).unwrap();
        let v =
            handle_profile_command("list", &json!({ "role_filter": "alpha" }), &conn).expect("ok");
        let arr = v.get("profiles").and_then(Value::as_array).unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0].get("name").and_then(Value::as_str), Some("alpha-1"));
    }

    #[test]
    fn list_tag_filter_filters_in_process() {
        let conn = fresh_conn();
        insert_agent_profile(&conn, &synth("with-x", "r", vec!["x", "y"])).unwrap();
        insert_agent_profile(&conn, &synth("with-y", "r", vec!["y"])).unwrap();
        let v = handle_profile_command("list", &json!({ "tag_filter": "x" }), &conn).expect("ok");
        let arr = v.get("profiles").and_then(Value::as_array).unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0].get("name").and_then(Value::as_str), Some("with-x"));
    }

    #[test]
    fn show_returns_full_profile_data() {
        let conn = fresh_conn();
        insert_agent_profile(&conn, &synth("p1", "r1", vec!["t1"])).unwrap();
        let v = handle_profile_command("show", &json!({ "name": "p1" }), &conn).expect("ok");
        assert_eq!(v.get("name").and_then(Value::as_str), Some("p1"));
        assert_eq!(v.get("role").and_then(Value::as_str), Some("r1"));
        assert_eq!(
            v.get("description").and_then(Value::as_str),
            Some("synth p1")
        );
    }

    #[test]
    fn show_without_name_returns_bad_params() {
        let conn = fresh_conn();
        let err = handle_profile_command("show", &json!({}), &conn).unwrap_err();
        assert!(matches!(err, ProfileHandlerError::BadParams(_)));
        assert_eq!(err.error_code(), "robot.profile.bad_params");
    }

    #[test]
    fn show_for_missing_profile_returns_not_found() {
        let conn = fresh_conn();
        let err = handle_profile_command("show", &json!({ "name": "ghost" }), &conn).unwrap_err();
        match err {
            ProfileHandlerError::NotFound { name } => assert_eq!(name, "ghost"),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn validate_returns_valid_for_well_formed_profile() {
        let conn = fresh_conn();
        insert_agent_profile(&conn, &synth("good", "r", vec![])).unwrap();
        let v = handle_profile_command("validate", &json!({ "name": "good" }), &conn).expect("ok");
        assert_eq!(v.get("valid").and_then(Value::as_bool), Some(true));
        let issues = v.get("issues").and_then(Value::as_array).unwrap();
        assert!(issues.is_empty());
    }

    #[test]
    fn validate_returns_invalid_with_reason_when_missing() {
        let conn = fresh_conn();
        let v = handle_profile_command("validate", &json!({ "name": "ghost" }), &conn).expect("ok");
        assert_eq!(v.get("valid").and_then(Value::as_bool), Some(false));
        let issues = v.get("issues").and_then(Value::as_array).unwrap();
        assert_eq!(issues.len(), 1);
        assert!(issues[0].as_str().unwrap().contains("not found"));
    }

    #[test]
    fn apply_dry_run_returns_plan_without_mutation() {
        let conn = fresh_conn();
        insert_agent_profile(&conn, &synth("ready", "r", vec![])).unwrap();
        let v = handle_profile_command(
            "apply",
            &json!({ "name": "ready", "count": 3, "dry_run": true }),
            &conn,
        )
        .expect("ok");
        assert_eq!(v.get("profile_name").and_then(Value::as_str), Some("ready"));
        assert_eq!(v.get("dry_run").and_then(Value::as_bool), Some(true));
        assert_eq!(
            v.get("panes_spawned")
                .and_then(Value::as_array)
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn apply_non_dry_run_returns_spawn_failed() {
        let conn = fresh_conn();
        insert_agent_profile(&conn, &synth("ready", "r", vec![])).unwrap();
        let err = handle_profile_command("apply", &json!({ "name": "ready" }), &conn).unwrap_err();
        match err {
            ProfileHandlerError::SpawnFailed { reason } => {
                assert!(reason.contains("ready"));
                assert!(reason.contains("count=1"));
                assert!(reason.contains("daemon-mediated pane spawning"));
            }
            other => panic!("expected SpawnFailed, got {other:?}"),
        }
    }

    #[test]
    fn live_apply_spawns_and_persists_receipt() {
        let conn = fresh_conn_with_apply_log();
        insert_agent_profile(&conn, &synth("ready", "r", vec![])).unwrap();
        let mut executor = FakeApplyExecutor::new();

        let v = handle_profile_apply_with_executor(
            &json!({ "name": "ready", "count": 2, "dry_run": false }),
            &conn,
            &mut executor,
            1_700_000_000_000,
        )
        .expect("live apply");

        assert_eq!(v.get("profile_name").and_then(Value::as_str), Some("ready"));
        assert_eq!(v.get("dry_run").and_then(Value::as_bool), Some(false));
        assert_eq!(
            v.get("panes_spawned").and_then(Value::as_array).unwrap(),
            &vec![json!(100), json!(101)]
        );
        assert_eq!(executor.spawned.len(), 2);
        assert!(executor.stopped.is_empty());
        assert_eq!(v.get("spawned_agents").and_then(Value::as_u64), Some(2));
    }

    #[test]
    fn live_apply_replays_existing_receipt_without_spawning() {
        let conn = fresh_conn_with_apply_log();
        insert_agent_profile(&conn, &synth("ready", "r", vec![])).unwrap();
        let mut executor = FakeApplyExecutor::new();
        let first = handle_profile_apply_with_executor(
            &json!({ "name": "ready", "count": 1, "dry_run": false }),
            &conn,
            &mut executor,
            1_700_000_000_000,
        )
        .expect("first apply");
        assert_eq!(executor.spawned.len(), 1);

        let mut replay_executor = FakeApplyExecutor::new();
        let replay = handle_profile_apply_with_executor(
            &json!({ "name": "ready", "count": 1, "dry_run": false }),
            &conn,
            &mut replay_executor,
            1_700_000_000_100,
        )
        .expect("replay apply");

        assert_eq!(
            first.get("panes_spawned"),
            replay.get("panes_spawned"),
            "replay returns original pane ids"
        );
        assert_eq!(
            replay.get("idempotent_replay").and_then(Value::as_bool),
            Some(true)
        );
        assert!(replay_executor.spawned.is_empty());
    }

    #[test]
    fn live_apply_policy_denial_does_not_spawn() {
        let conn = fresh_conn_with_apply_log();
        insert_agent_profile(&conn, &synth("ready", "r", vec![])).unwrap();
        let mut executor = FakeApplyExecutor::new();
        executor.policy = Some(FleetMutationPolicyDecision::Deny {
            reason_code: "blocked".to_string(),
            message: "profile launches disabled".to_string(),
        });

        let err = handle_profile_apply_with_executor(
            &json!({ "name": "ready", "count": 1, "dry_run": false }),
            &conn,
            &mut executor,
            1_700_000_000_000,
        )
        .unwrap_err();

        assert!(matches!(err, ProfileHandlerError::PolicyDenied { .. }));
        assert!(executor.spawned.is_empty());
    }

    #[test]
    fn live_apply_partial_failure_compensates_prior_spawns() {
        let conn = fresh_conn_with_apply_log();
        insert_agent_profile(&conn, &synth("ready", "r", vec![])).unwrap();
        let mut executor = FakeApplyExecutor::new();
        executor.fail_on_ordinal = Some(2);

        let err = handle_profile_apply_with_executor(
            &json!({ "name": "ready", "count": 3, "dry_run": false }),
            &conn,
            &mut executor,
            1_700_000_000_000,
        )
        .unwrap_err();

        match err {
            ProfileHandlerError::ApplyFailed { error_code, reason } => {
                assert_eq!(error_code, "robot.profile.mux_spawn_failed");
                assert!(reason.contains("spawn 2 failed"));
            }
            other => panic!("expected ApplyFailed, got {other:?}"),
        }
        assert_eq!(executor.spawned.len(), 2);
        assert_eq!(executor.stopped.len(), 1);
        assert_eq!(executor.stopped[0].0, 100);
    }

    #[test]
    fn live_apply_compensation_failure_is_specific() {
        let conn = fresh_conn_with_apply_log();
        insert_agent_profile(&conn, &synth("ready", "r", vec![])).unwrap();
        let mut executor = FakeApplyExecutor::new();
        executor.fail_on_ordinal = Some(2);
        executor.fail_stop = true;

        let err = handle_profile_apply_with_executor(
            &json!({ "name": "ready", "count": 2, "dry_run": false }),
            &conn,
            &mut executor,
            1_700_000_000_000,
        )
        .unwrap_err();

        assert!(matches!(
            err,
            ProfileHandlerError::CompensationFailed { .. }
        ));
        assert_eq!(executor.stopped.len(), 1);
    }

    #[test]
    fn live_apply_daemon_unavailable_surfaces_executor_code() {
        let conn = fresh_conn_with_apply_log();
        insert_agent_profile(&conn, &synth("ready", "r", vec![])).unwrap();
        let mut executor = FakeApplyExecutor::new();
        executor.fail_on_ordinal = Some(1);

        let err = handle_profile_apply_with_executor(
            &json!({ "name": "ready", "count": 1, "dry_run": false }),
            &conn,
            &mut executor,
            1_700_000_000_000,
        )
        .unwrap_err();

        assert_eq!(err.error_code(), "robot.profile.mux_spawn_failed");
    }

    #[test]
    fn apply_for_missing_profile_returns_not_found_before_spawn_check() {
        let conn = fresh_conn();
        let err = handle_profile_command("apply", &json!({ "name": "ghost" }), &conn).unwrap_err();
        assert!(matches!(err, ProfileHandlerError::NotFound { .. }));
    }

    #[test]
    fn unknown_action_returns_typed_error() {
        let conn = fresh_conn();
        let err = handle_profile_command("noop", &json!({}), &conn).unwrap_err();
        match err {
            ProfileHandlerError::UnknownAction(a) => assert_eq!(a, "noop"),
            other => panic!("expected UnknownAction, got {other:?}"),
        }
    }

    #[test]
    fn list_is_deterministic() {
        // Two consecutive calls on the same DB must return identical
        // bytes — the contract's `list_is_deterministic` invariant.
        let conn = fresh_conn();
        insert_agent_profile(&conn, &synth("a", "r", vec![])).unwrap();
        insert_agent_profile(&conn, &synth("b", "r", vec![])).unwrap();
        let v1 = handle_profile_command("list", &json!({}), &conn).unwrap();
        let v2 = handle_profile_command("list", &json!({}), &conn).unwrap();
        assert_eq!(v1, v2);
    }

    #[test]
    fn show_is_deterministic() {
        let conn = fresh_conn();
        insert_agent_profile(&conn, &synth("p", "r", vec!["t"])).unwrap();
        let v1 = handle_profile_command("show", &json!({ "name": "p" }), &conn).unwrap();
        let v2 = handle_profile_command("show", &json!({ "name": "p" }), &conn).unwrap();
        assert_eq!(v1, v2);
    }

    #[test]
    fn error_code_maps_each_variant() {
        assert_eq!(
            ProfileHandlerError::UnknownAction("x".into()).error_code(),
            "robot.profile.unknown_action",
        );
        assert_eq!(
            ProfileHandlerError::BadParams("x".into()).error_code(),
            "robot.profile.bad_params",
        );
        assert_eq!(
            ProfileHandlerError::NotFound { name: "x".into() }.error_code(),
            "robot.profile.not_found",
        );
        assert_eq!(
            ProfileHandlerError::SpawnFailed { reason: "x".into() }.error_code(),
            "robot.profile.spawn_failed",
        );
    }
}

// br-ft-iwg7x: serialize tests that touch the process-global
// `ROBOT_PROFILE_BOOTSTRAP_SERDE_DROP_COUNT`. Other tests in this
// crate may run in parallel and reset/observe the same atomic, so
// we serialize through a module-local Mutex to avoid flakes.
#[cfg(test)]
mod bootstrap_serde_drop_tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    static SERDE_DROP_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn lock() -> MutexGuard<'static, ()> {
        SERDE_DROP_TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    #[test]
    fn missing_key_does_not_bump_counter() {
        let _g = lock();
        reset_robot_profile_bootstrap_serde_drop_count_for_test();
        let metadata: HashMap<String, String> = HashMap::new();
        let result = parse_bootstrap_commands(&metadata);
        assert!(result.is_empty());
        assert_eq!(robot_profile_bootstrap_serde_drop_count(), 0);
    }

    #[test]
    fn well_formed_array_parses_without_bump() {
        let _g = lock();
        reset_robot_profile_bootstrap_serde_drop_count_for_test();
        let mut metadata = HashMap::new();
        metadata.insert(
            "bootstrap_commands".to_string(),
            r#"["echo hi","ls -la"]"#.to_string(),
        );
        let result = parse_bootstrap_commands(&metadata);
        assert_eq!(result, vec!["echo hi".to_string(), "ls -la".to_string()]);
        assert_eq!(robot_profile_bootstrap_serde_drop_count(), 0);
    }

    #[test]
    fn malformed_json_bumps_counter() {
        let _g = lock();
        reset_robot_profile_bootstrap_serde_drop_count_for_test();
        let mut metadata = HashMap::new();
        // unterminated array
        metadata.insert(
            "bootstrap_commands".to_string(),
            r#"["echo hi""#.to_string(),
        );
        let result = parse_bootstrap_commands(&metadata);
        assert!(result.is_empty());
        assert_eq!(robot_profile_bootstrap_serde_drop_count(), 1);
    }

    #[test]
    fn wrong_shape_object_bumps_counter() {
        let _g = lock();
        reset_robot_profile_bootstrap_serde_drop_count_for_test();
        let mut metadata = HashMap::new();
        // valid JSON but not Vec<String>
        metadata.insert(
            "bootstrap_commands".to_string(),
            r#"{"a": "b"}"#.to_string(),
        );
        let result = parse_bootstrap_commands(&metadata);
        assert!(result.is_empty());
        assert_eq!(robot_profile_bootstrap_serde_drop_count(), 1);
    }

    #[test]
    fn array_of_non_strings_bumps_counter() {
        let _g = lock();
        reset_robot_profile_bootstrap_serde_drop_count_for_test();
        let mut metadata = HashMap::new();
        // Vec<i32>, not Vec<String>
        metadata.insert("bootstrap_commands".to_string(), r"[1, 2, 3]".to_string());
        let result = parse_bootstrap_commands(&metadata);
        assert!(result.is_empty());
        assert_eq!(robot_profile_bootstrap_serde_drop_count(), 1);
    }

    #[test]
    fn repeated_malformed_inputs_bump_monotonically() {
        let _g = lock();
        reset_robot_profile_bootstrap_serde_drop_count_for_test();
        let mut metadata = HashMap::new();
        metadata.insert(
            "bootstrap_commands".to_string(),
            "not even json".to_string(),
        );
        for _ in 0..5 {
            let _ = parse_bootstrap_commands(&metadata);
        }
        assert_eq!(robot_profile_bootstrap_serde_drop_count(), 5);
    }

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config::with_cases(64))]

        // br-ft-iwg7x: any non-Vec<String> JSON document must bump
        // the counter exactly once and yield an empty Vec.
        #[test]
        fn arbitrary_non_array_json_always_bumps(
            shape in proptest::sample::select(vec![
                "null".to_string(),
                "true".to_string(),
                "false".to_string(),
                "42".to_string(),
                "\"a string not an array\"".to_string(),
                "{}".to_string(),
                "{\"k\":\"v\"}".to_string(),
                "[1,2,3]".to_string(),
                "[true]".to_string(),
                "[null]".to_string(),
                "[[\"nested\"]]".to_string(),
                "[\"valid\", 1]".to_string(),
            ]),
        ) {
            let _g = lock();
            reset_robot_profile_bootstrap_serde_drop_count_for_test();
            let mut metadata = HashMap::new();
            metadata.insert("bootstrap_commands".to_string(), shape);
            let result = parse_bootstrap_commands(&metadata);
            proptest::prop_assert!(result.is_empty());
            proptest::prop_assert_eq!(robot_profile_bootstrap_serde_drop_count(), 1);
        }

        // br-ft-iwg7x: any Vec<String> serialization round-trips
        // and never bumps the counter.
        #[test]
        fn arbitrary_string_vec_does_not_bump(
            commands in proptest::collection::vec("[a-zA-Z0-9 ._/-]{0,32}", 0..8),
        ) {
            let _g = lock();
            reset_robot_profile_bootstrap_serde_drop_count_for_test();
            let raw = serde_json::to_string(&commands).unwrap();
            let mut metadata = HashMap::new();
            metadata.insert("bootstrap_commands".to_string(), raw);
            let result = parse_bootstrap_commands(&metadata);
            proptest::prop_assert_eq!(&result, &commands);
            proptest::prop_assert_eq!(robot_profile_bootstrap_serde_drop_count(), 0);
        }
    }
}
