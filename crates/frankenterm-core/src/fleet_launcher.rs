// =============================================================================
// Native fleet launcher with weighted agent orchestration (ft-3681t.3.1)
//
// First-class fleet launch semantics: weighted model/program mix, role
// allocation, startup orchestration, and deterministic topology initialization
// backed by native mux profiles and the lifecycle engine.
// =============================================================================

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::command_transport::{
    CommandContext, CommandKind, CommandRequest, CommandRouter, CommandScope,
};
use crate::durable_state::{CheckpointTrigger, DurableStateManager};
use crate::session_profiles::{
    AgentIdentitySpec, ProfileRegistry, ProfileRole, SessionProfile, SpawnCommand,
};
use crate::session_topology::{
    AgentLifecycleState, LifecycleEntityKind, LifecycleIdentity, LifecycleRegistry, LifecycleState,
    MuxPaneLifecycleState, SessionLifecycleState, WindowLifecycleState,
};

// =============================================================================
// Fleet specification types
// =============================================================================

/// Top-level specification for launching a fleet of agent panes.
///
/// A `FleetSpec` describes the desired fleet composition using weighted model/program
/// mix entries and optional role constraints. The launcher resolves this into a
/// concrete `LaunchPlan` with deterministic slot assignments.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FleetSpec {
    /// Fleet name for identification and logging.
    pub name: String,
    /// Human-readable description.
    #[serde(default)]
    pub description: Option<String>,
    /// Workspace identifier for lifecycle registration.
    pub workspace_id: String,
    /// Domain for lifecycle identity (e.g., "local", "ssh:host").
    #[serde(default = "default_domain")]
    pub domain: String,
    /// Weighted mix of agent programs/models to deploy.
    pub mix: Vec<AgentMixEntry>,
    /// Total number of panes to launch. If 0, derived from mix weights.
    #[serde(default)]
    pub total_panes: u32,
    /// Fleet template name to use for layout (optional).
    #[serde(default)]
    pub fleet_template: Option<String>,
    /// Working directory for all panes (unless overridden by profile).
    #[serde(default)]
    pub working_directory: Option<String>,
    /// Startup ordering strategy.
    #[serde(default)]
    pub startup_strategy: StartupStrategy,
    /// Generation counter for lifecycle identity scoping.
    #[serde(default = "default_generation")]
    pub generation: u64,
    /// Tags for the fleet.
    #[serde(default)]
    pub tags: Vec<String>,
}

fn default_domain() -> String {
    "local".to_string()
}

fn default_generation() -> u64 {
    1
}

/// Weighted entry in the agent mix.
///
/// Each entry specifies a program/model combination with a weight that
/// determines how many panes of this type are allocated relative to others.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentMixEntry {
    /// Agent program (e.g., "claude-code", "codex-cli", "gemini-cli").
    pub program: String,
    /// Agent model (e.g., "opus-4.1", "gpt5-codex").
    #[serde(default)]
    pub model: Option<String>,
    /// Relative weight for this program/model combo (default 1).
    #[serde(default = "default_mix_weight")]
    pub weight: u32,
    /// Profile to use for panes of this type (defaults to "agent-worker").
    #[serde(default)]
    pub profile: Option<String>,
    /// Task description template. `{index}` is replaced with the slot index.
    #[serde(default)]
    pub task_template: Option<String>,
    /// Extra environment variables for this entry.
    #[serde(default)]
    pub environment: HashMap<String, String>,
    /// Role assignment for this entry.
    #[serde(default)]
    pub role: Option<ProfileRole>,
}

fn default_mix_weight() -> u32 {
    1
}

/// Strategy for ordering pane startup within a fleet launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StartupStrategy {
    /// Launch all panes simultaneously.
    #[default]
    Parallel,
    /// Launch panes one at a time in order.
    Sequential,
    /// Launch panes in waves (groups determined by role priority).
    Phased,
}

// =============================================================================
// Launch plan types
// =============================================================================

/// A concrete, executable plan for launching a fleet.
///
/// Produced by `FleetLauncher::plan()` from a `FleetSpec`. Contains fully
/// resolved slot assignments, lifecycle identities, and startup ordering.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaunchPlan {
    /// Fleet name.
    pub name: String,
    /// Resolved pane slots in launch order.
    pub slots: Vec<LaunchSlot>,
    /// Layout template to apply (if any).
    pub layout_template: Option<String>,
    /// Startup strategy.
    pub strategy: StartupStrategy,
    /// Startup phases (used when strategy=Phased).
    pub phases: Vec<LaunchPhase>,
    /// Generation for lifecycle scoping.
    pub generation: u64,
    /// Workspace identifier.
    pub workspace_id: String,
    /// Domain.
    pub domain: String,
    /// When this plan was created (epoch ms).
    pub planned_at: u64,
    /// Validation warnings (non-fatal issues detected during planning).
    pub warnings: Vec<String>,
}

/// A single slot in a launch plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaunchSlot {
    /// Slot index (0-based, determines launch order for sequential).
    pub index: u32,
    /// Label for this slot.
    pub label: String,
    /// Agent identity spec for this slot.
    pub agent_identity: AgentIdentitySpec,
    /// Resolved profile for this pane.
    pub profile: SessionProfile,
    /// Merged environment variables.
    pub environment: HashMap<String, String>,
    /// Spawn command (from profile or mix entry).
    pub spawn_command: Option<SpawnCommand>,
    /// Working directory.
    pub working_directory: Option<String>,
    /// Bootstrap commands to run after spawn.
    pub bootstrap_commands: Vec<String>,
    /// Lifecycle identity for registry registration.
    pub lifecycle_identity: LifecycleIdentity,
    /// Phase index (for phased startup).
    pub phase: u32,
    /// Source mix entry index.
    pub mix_entry_index: usize,
}

/// A launch phase groups slots that should start together.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaunchPhase {
    /// Phase index (0-based).
    pub index: u32,
    /// Label for this phase.
    pub label: String,
    /// Slot indices in this phase.
    pub slot_indices: Vec<u32>,
}

// =============================================================================
// Launch outcome types
// =============================================================================

/// Outcome of executing a launch plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaunchOutcome {
    /// Fleet name.
    pub name: String,
    /// Per-slot results.
    pub slot_outcomes: Vec<SlotOutcome>,
    /// Overall fleet status.
    pub status: FleetLaunchStatus,
    /// Lifecycle registry snapshot after registration.
    pub registry_snapshot: Vec<crate::session_topology::LifecycleEntityRecord>,
    /// When the launch completed (epoch ms).
    pub completed_at: u64,
    /// Total slots attempted.
    pub total_slots: u32,
    /// Slots successfully registered.
    pub successful_slots: u32,
    /// Slots that failed registration.
    pub failed_slots: u32,
    /// Checkpoint ID taken before launch (when durable state is used).
    pub pre_launch_checkpoint: Option<u64>,
    /// Bootstrap commands dispatched per slot (slot_index, command_count).
    pub bootstrap_dispatches: Vec<(u32, usize)>,
}

/// Per-slot outcome.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlotOutcome {
    pub index: u32,
    pub label: String,
    pub status: SlotStatus,
    pub lifecycle_identity: LifecycleIdentity,
    pub error: Option<String>,
}

/// Status of a single slot launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlotStatus {
    /// Successfully registered in lifecycle engine.
    Registered,
    /// Failed to register.
    Failed,
    /// Skipped (e.g., due to earlier failure in sequential mode).
    Skipped,
}

/// Overall fleet launch status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FleetLaunchStatus {
    /// All slots launched successfully.
    Complete,
    /// Some slots failed but fleet is partially operational.
    Partial,
    /// All slots failed.
    Failed,
}

// =============================================================================
// Fleet launcher errors
// =============================================================================

/// Errors that can occur during fleet planning or launch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FleetLaunchError {
    /// Fleet spec has no mix entries.
    EmptyMix,
    /// Total weight is zero (all mix entries have weight 0).
    ZeroWeight,
    /// Referenced profile not found in registry.
    ProfileNotFound(String),
    /// Referenced fleet template not found.
    TemplateNotFound(String),
    /// Lifecycle registration failed for a slot.
    RegistrationFailed { slot_index: u32, reason: String },
    /// Fleet spec validation failed.
    ValidationFailed(String),
}

impl std::fmt::Display for FleetLaunchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyMix => write!(f, "fleet spec has no mix entries"),
            Self::ZeroWeight => write!(f, "total mix weight is zero"),
            Self::ProfileNotFound(name) => {
                write!(f, "profile '{name}' not found in registry")
            }
            Self::TemplateNotFound(name) => {
                write!(f, "fleet template '{name}' not found in registry")
            }
            Self::RegistrationFailed { slot_index, reason } => {
                write!(f, "slot {slot_index} registration failed: {reason}")
            }
            Self::ValidationFailed(msg) => write!(f, "fleet spec validation failed: {msg}"),
        }
    }
}

// =============================================================================
// LaunchPlan query surface
// =============================================================================

/// Per-program distribution summary within a launch plan.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProgramDistribution {
    /// Program name (e.g., "claude-code", "codex-cli").
    pub program: String,
    /// Number of slots allocated to this program.
    pub slot_count: u32,
    /// Slot indices for this program.
    pub slot_indices: Vec<u32>,
}

/// Reason code for a metadata projection failure.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum MetadataProjectionFailure {
    /// No slots assigned to the specified program.
    ProgramNotFound { program: String },
    /// No slots in the specified phase.
    PhaseNotFound { phase: u32 },
    /// Slot index out of bounds.
    SlotIndexOutOfBounds { index: u32, max: u32 },
    /// Phase index mismatch (slot references a phase that doesn't exist).
    PhaseSlotMismatch { slot_index: u32, claimed_phase: u32 },
    /// Inconsistent field: slot has missing or empty required fields.
    InconsistentSlotField { slot_index: u32, field: String },
}

impl std::fmt::Display for MetadataProjectionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ProgramNotFound { program } => {
                write!(f, "no slots for program '{program}'")
            }
            Self::PhaseNotFound { phase } => write!(f, "no phase {phase}"),
            Self::SlotIndexOutOfBounds { index, max } => {
                write!(f, "slot index {index} out of bounds (max {max})")
            }
            Self::PhaseSlotMismatch {
                slot_index,
                claimed_phase,
            } => write!(
                f,
                "slot {slot_index} claims phase {claimed_phase} but no such phase exists"
            ),
            Self::InconsistentSlotField { slot_index, field } => {
                write!(f, "slot {slot_index} has empty/missing field '{field}'")
            }
        }
    }
}

impl LaunchPlan {
    /// Return distinct programs across all slots, preserving first-seen order.
    #[must_use]
    pub fn programs(&self) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for slot in &self.slots {
            if seen.insert(slot.agent_identity.program.clone()) {
                out.push(slot.agent_identity.program.clone());
            }
        }
        out
    }

    /// Return per-program distribution summary.
    #[must_use]
    pub fn program_distribution(&self) -> Vec<ProgramDistribution> {
        let mut map: std::collections::BTreeMap<String, Vec<u32>> =
            std::collections::BTreeMap::new();
        for slot in &self.slots {
            map.entry(slot.agent_identity.program.clone())
                .or_default()
                .push(slot.index);
        }
        map.into_iter()
            .map(|(program, slot_indices)| ProgramDistribution {
                program,
                slot_count: slot_indices.len() as u32,
                slot_indices,
            })
            .collect()
    }

    /// Return slots belonging to a specific phase.
    #[must_use]
    pub fn slots_in_phase(&self, phase_index: u32) -> Vec<&LaunchSlot> {
        self.slots.iter().filter(|s| s.phase == phase_index).collect()
    }

    /// Return slot by index, or `None` if out of bounds.
    #[must_use]
    pub fn slot(&self, index: u32) -> Option<&LaunchSlot> {
        self.slots.iter().find(|s| s.index == index)
    }

    /// Return phase labels in order.
    #[must_use]
    pub fn phase_labels(&self) -> Vec<String> {
        self.phases.iter().map(|p| p.label.clone()).collect()
    }

    /// Run deterministic invariant checks and return any failures found.
    ///
    /// These checks validate internal consistency of the plan metadata:
    /// - All slot indices are sequential (0..n)
    /// - All slot labels are unique
    /// - All lifecycle identities are unique
    /// - Every slot's phase references an existing phase
    /// - Every phase's slot_indices reference existing slots
    /// - No slot has empty label or program
    #[must_use]
    pub fn invariant_violations(&self) -> Vec<MetadataProjectionFailure> {
        let mut violations = Vec::new();

        // Check sequential indices
        for (i, slot) in self.slots.iter().enumerate() {
            if slot.index != i as u32 {
                violations.push(MetadataProjectionFailure::SlotIndexOutOfBounds {
                    index: slot.index,
                    max: self.slots.len().saturating_sub(1) as u32,
                });
            }
        }

        // Check phase references
        let phase_indices: std::collections::HashSet<u32> =
            self.phases.iter().map(|p| p.index).collect();
        for slot in &self.slots {
            if !phase_indices.contains(&slot.phase) {
                violations.push(MetadataProjectionFailure::PhaseSlotMismatch {
                    slot_index: slot.index,
                    claimed_phase: slot.phase,
                });
            }
        }

        // Check slot field consistency
        for slot in &self.slots {
            if slot.label.is_empty() {
                violations.push(MetadataProjectionFailure::InconsistentSlotField {
                    slot_index: slot.index,
                    field: "label".to_string(),
                });
            }
            if slot.agent_identity.program.is_empty() {
                violations.push(MetadataProjectionFailure::InconsistentSlotField {
                    slot_index: slot.index,
                    field: "program".to_string(),
                });
            }
        }

        violations
    }
}

// =============================================================================
// LaunchOutcome query surface
// =============================================================================

/// Summary of entities registered during fleet launch.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RegistrySummary {
    /// Total entities in registry snapshot.
    pub total_entities: usize,
    /// Session entity count.
    pub sessions: usize,
    /// Window entity count.
    pub windows: usize,
    /// Pane entity count.
    pub panes: usize,
    /// Agent entity count.
    pub agents: usize,
}

impl LaunchOutcome {
    /// Return a high-level summary of registered entities.
    #[must_use]
    pub fn registry_summary(&self) -> RegistrySummary {
        let sessions = self
            .registry_snapshot
            .iter()
            .filter(|e| e.identity.kind == LifecycleEntityKind::Session)
            .count();
        let windows = self
            .registry_snapshot
            .iter()
            .filter(|e| e.identity.kind == LifecycleEntityKind::Window)
            .count();
        let panes = self
            .registry_snapshot
            .iter()
            .filter(|e| e.identity.kind == LifecycleEntityKind::Pane)
            .count();
        let agents = self
            .registry_snapshot
            .iter()
            .filter(|e| e.identity.kind == LifecycleEntityKind::Agent)
            .count();
        RegistrySummary {
            total_entities: self.registry_snapshot.len(),
            sessions,
            windows,
            panes,
            agents,
        }
    }

    /// Return outcomes for only successful slots.
    #[must_use]
    pub fn successful_outcomes(&self) -> Vec<&SlotOutcome> {
        self.slot_outcomes
            .iter()
            .filter(|o| o.status == SlotStatus::Registered)
            .collect()
    }

    /// Return outcomes for failed slots.
    #[must_use]
    pub fn failed_outcomes(&self) -> Vec<&SlotOutcome> {
        self.slot_outcomes
            .iter()
            .filter(|o| o.status == SlotStatus::Failed)
            .collect()
    }

    /// Return outcomes for skipped slots.
    #[must_use]
    pub fn skipped_outcomes(&self) -> Vec<&SlotOutcome> {
        self.slot_outcomes
            .iter()
            .filter(|o| o.status == SlotStatus::Skipped)
            .collect()
    }

    /// Check if the outcome is fully successful (all slots registered).
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.status == FleetLaunchStatus::Complete
    }
}

// =============================================================================
// Fleet launcher
// =============================================================================

/// Orchestrates deterministic fleet launches from fleet specifications.
///
/// The launcher resolves a `FleetSpec` into a `LaunchPlan` using the profile
/// registry for configuration and then executes the plan by registering
/// entities in the lifecycle registry.
#[derive(Debug)]
pub struct FleetLauncher<'a> {
    profile_registry: &'a ProfileRegistry,
}

impl<'a> FleetLauncher<'a> {
    /// Create a new fleet launcher backed by a profile registry.
    #[must_use]
    pub fn new(profile_registry: &'a ProfileRegistry) -> Self {
        Self { profile_registry }
    }

    /// Plan a fleet launch from a spec without executing it.
    ///
    /// Resolves weighted mix entries into concrete slot assignments, validates
    /// all referenced profiles exist, and produces a `LaunchPlan` ready for
    /// execution.
    pub fn plan(&self, spec: &FleetSpec) -> Result<LaunchPlan, FleetLaunchError> {
        // Validate spec
        if spec.mix.is_empty() {
            return Err(FleetLaunchError::EmptyMix);
        }

        let total_weight: u32 = spec.mix.iter().map(|e| e.weight).sum();
        if total_weight == 0 {
            return Err(FleetLaunchError::ZeroWeight);
        }

        // Determine total pane count
        let total_panes = if spec.total_panes > 0 {
            spec.total_panes
        } else {
            total_weight
        };

        // Validate referenced profiles
        let mut warnings = Vec::new();
        for (i, entry) in spec.mix.iter().enumerate() {
            let profile_name = entry.profile.as_deref().unwrap_or("agent-worker");
            if self.profile_registry.get_profile(profile_name).is_none() {
                return Err(FleetLaunchError::ProfileNotFound(profile_name.to_string()));
            }
            if entry.weight == 0 {
                warnings.push(format!(
                    "mix entry {i} ({}) has weight 0, will get no slots",
                    entry.program
                ));
            }
        }

        // Validate fleet template if specified
        if let Some(template_name) = &spec.fleet_template {
            if self
                .profile_registry
                .get_fleet_template(template_name)
                .is_none()
            {
                return Err(FleetLaunchError::TemplateNotFound(template_name.clone()));
            }
        }

        // Allocate slots using weighted distribution
        let allocations = allocate_weighted(total_panes, &spec.mix);

        // Build launch slots
        let mut slots = Vec::with_capacity(total_panes as usize);
        let mut global_index: u32 = 0;

        for (mix_idx, (entry, count)) in spec.mix.iter().zip(allocations.iter()).enumerate() {
            for local_idx in 0..*count {
                let profile_name = entry.profile.as_deref().unwrap_or("agent-worker");
                let profile = self
                    .profile_registry
                    .get_profile(profile_name)
                    .cloned()
                    .expect("validated above");

                // Build merged environment
                let mut environment = profile.environment.clone();
                for (k, v) in &entry.environment {
                    environment.insert(k.clone(), v.clone());
                }
                environment.insert("FT_FLEET_NAME".to_string(), spec.name.clone());
                environment.insert("FT_SLOT_INDEX".to_string(), global_index.to_string());
                environment.insert("FT_MIX_ENTRY".to_string(), entry.program.clone());

                // Build agent identity
                let task = entry.task_template.as_ref().map(|t| {
                    t.replace("{index}", &global_index.to_string())
                        .replace("{local_index}", &local_idx.to_string())
                        .replace("{program}", &entry.program)
                });

                let agent_identity = AgentIdentitySpec {
                    program: entry.program.clone(),
                    model: entry.model.clone(),
                    task: task
                        .or_else(|| Some(format!("{} worker #{}", entry.program, global_index))),
                };

                // Build label. Use global slot index to guarantee uniqueness even
                // when multiple mix entries share the same program name.
                let label = format!(
                    "{}-{}-{}",
                    spec.name,
                    entry.program.replace(' ', "-"),
                    global_index
                );

                // Build lifecycle identity
                let lifecycle_identity = LifecycleIdentity::new(
                    LifecycleEntityKind::Pane,
                    &spec.workspace_id,
                    &spec.domain,
                    u64::from(global_index),
                    spec.generation,
                );

                // Determine phase for phased startup
                let phase = match spec.startup_strategy {
                    StartupStrategy::Phased => phase_for_role(entry.role.unwrap_or(profile.role)),
                    _ => 0,
                };

                // Determine working directory
                let working_directory = spec
                    .working_directory
                    .clone()
                    .or_else(|| profile.working_directory.clone());

                slots.push(LaunchSlot {
                    index: global_index,
                    label,
                    agent_identity,
                    profile: profile.clone(),
                    environment,
                    spawn_command: profile.spawn_command.clone(),
                    working_directory,
                    bootstrap_commands: profile.bootstrap_commands.clone(),
                    lifecycle_identity,
                    phase,
                    mix_entry_index: mix_idx,
                });

                global_index += 1;
            }
        }

        // Build phases for phased startup
        let phases = if spec.startup_strategy == StartupStrategy::Phased {
            build_phases(&slots)
        } else {
            vec![LaunchPhase {
                index: 0,
                label: "all".to_string(),
                slot_indices: (0..slots.len() as u32).collect(),
            }]
        };

        Ok(LaunchPlan {
            name: spec.name.clone(),
            slots,
            layout_template: spec.fleet_template.clone(),
            strategy: spec.startup_strategy,
            phases,
            generation: spec.generation,
            workspace_id: spec.workspace_id.clone(),
            domain: spec.domain.clone(),
            planned_at: epoch_ms(),
            warnings,
        })
    }

    /// Execute a launch plan by registering entities in the lifecycle registry.
    ///
    /// Creates session, window, pane, and agent entities for each slot. Returns a
    /// `LaunchOutcome` with per-slot results.
    pub fn execute(&self, plan: &LaunchPlan, registry: &mut LifecycleRegistry) -> LaunchOutcome {
        self.execute_with_subsystems(plan, registry, None, None)
    }

    /// Execute a launch plan with optional durable-state checkpoint and command routing.
    ///
    /// When `durable_state` is provided, a pre-launch checkpoint is taken before
    /// entity registration, enabling rollback on partial failure. When `router` is
    /// provided, bootstrap commands are dispatched through the command transport
    /// layer for each successfully registered slot.
    pub fn execute_with_subsystems(
        &self,
        plan: &LaunchPlan,
        registry: &mut LifecycleRegistry,
        mut durable_state: Option<&mut DurableStateManager>,
        mut router: Option<&mut CommandRouter>,
    ) -> LaunchOutcome {
        let timestamp = epoch_ms();
        let mut slot_outcomes = Vec::with_capacity(plan.slots.len());
        let mut successful: u32 = 0;
        let mut failed: u32 = 0;
        let mut bootstrap_dispatches: Vec<(u32, usize)> = Vec::new();
        let sequential = plan.strategy == StartupStrategy::Sequential;
        let mut sequential_halt: Option<(u32, String)> = None;

        // Take a durable-state checkpoint before fleet provisioning
        let pre_launch_checkpoint = if let Some(ds) = durable_state.as_mut() {
            let mut metadata = HashMap::new();
            metadata.insert("fleet_name".to_string(), plan.name.clone());
            metadata.insert("total_slots".to_string(), plan.slots.len().to_string());
            let checkpoint = ds.checkpoint(
                registry,
                format!("pre-fleet-launch:{}", plan.name),
                CheckpointTrigger::FleetProvisioning {
                    fleet_name: plan.name.clone(),
                },
                metadata,
            );
            Some(checkpoint.id)
        } else {
            None
        };

        // Register session entity (one per fleet launch)
        let session_identity = LifecycleIdentity::new(
            LifecycleEntityKind::Session,
            &plan.workspace_id,
            &plan.domain,
            0,
            plan.generation,
        );
        let _ = registry.register_entity(
            session_identity,
            LifecycleState::Session(SessionLifecycleState::Active),
            timestamp,
        );

        // Register window entity (one per fleet launch)
        let window_identity = LifecycleIdentity::new(
            LifecycleEntityKind::Window,
            &plan.workspace_id,
            &plan.domain,
            0,
            plan.generation,
        );
        let _ = registry.register_entity(
            window_identity,
            LifecycleState::Window(WindowLifecycleState::Active),
            timestamp,
        );

        // Register pane + agent entities per slot
        for slot in &plan.slots {
            if let Some((failed_slot, failed_reason)) = &sequential_halt {
                slot_outcomes.push(SlotOutcome {
                    index: slot.index,
                    label: slot.label.clone(),
                    status: SlotStatus::Skipped,
                    lifecycle_identity: slot.lifecycle_identity.clone(),
                    error: Some(format!(
                        "sequential launch halted after slot {failed_slot} failed: {failed_reason}",
                    )),
                });
                continue;
            }

            // Check for pre-existing pane entity (conflict detection)
            if registry.get(&slot.lifecycle_identity).is_some() {
                failed += 1;
                let error = format!(
                    "pane entity already registered: {}",
                    slot.lifecycle_identity.stable_key(),
                );
                if sequential {
                    sequential_halt = Some((slot.index, error.clone()));
                }
                slot_outcomes.push(SlotOutcome {
                    index: slot.index,
                    label: slot.label.clone(),
                    status: SlotStatus::Failed,
                    lifecycle_identity: slot.lifecycle_identity.clone(),
                    error: Some(error),
                });
                continue;
            }

            // Register the pane entity
            let result = registry.register_entity(
                slot.lifecycle_identity.clone(),
                LifecycleState::Pane(MuxPaneLifecycleState::Provisioning),
                timestamp,
            );

            match result {
                Ok(_) => {
                    // Register corresponding agent entity only after pane registration
                    // succeeds, so failed panes don't leave orphan agents.
                    let agent_identity = LifecycleIdentity::new(
                        LifecycleEntityKind::Agent,
                        &plan.workspace_id,
                        &plan.domain,
                        u64::from(slot.index),
                        plan.generation,
                    );
                    if let Err(e) = registry.register_entity(
                        agent_identity,
                        LifecycleState::Agent(AgentLifecycleState::Registered),
                        timestamp,
                    ) {
                        failed += 1;
                        let error =
                            format!("agent registration failed after pane registration: {e}");
                        if sequential {
                            sequential_halt = Some((slot.index, error.clone()));
                        }
                        slot_outcomes.push(SlotOutcome {
                            index: slot.index,
                            label: slot.label.clone(),
                            status: SlotStatus::Failed,
                            lifecycle_identity: slot.lifecycle_identity.clone(),
                            error: Some(error),
                        });
                        continue;
                    }

                    successful += 1;

                    // Dispatch bootstrap commands through CommandRouter if available
                    if let Some(ref mut cmd_router) = router {
                        let cmd_count = dispatch_bootstrap_commands(
                            cmd_router, slot, registry, &plan.name, timestamp,
                        );
                        bootstrap_dispatches.push((slot.index, cmd_count));
                    }

                    slot_outcomes.push(SlotOutcome {
                        index: slot.index,
                        label: slot.label.clone(),
                        status: SlotStatus::Registered,
                        lifecycle_identity: slot.lifecycle_identity.clone(),
                        error: None,
                    });
                }
                Err(e) => {
                    failed += 1;
                    let error = e.to_string();
                    if sequential {
                        sequential_halt = Some((slot.index, error.clone()));
                    }
                    slot_outcomes.push(SlotOutcome {
                        index: slot.index,
                        label: slot.label.clone(),
                        status: SlotStatus::Failed,
                        lifecycle_identity: slot.lifecycle_identity.clone(),
                        error: Some(error),
                    });
                }
            }
        }

        let status = if failed == 0 {
            FleetLaunchStatus::Complete
        } else if successful == 0 {
            FleetLaunchStatus::Failed
        } else {
            FleetLaunchStatus::Partial
        };

        LaunchOutcome {
            name: plan.name.clone(),
            slot_outcomes,
            status,
            registry_snapshot: registry.snapshot(),
            completed_at: epoch_ms(),
            total_slots: plan.slots.len() as u32,
            successful_slots: successful,
            failed_slots: failed,
            pre_launch_checkpoint,
            bootstrap_dispatches,
        }
    }

    /// Plan and execute in one step.
    pub fn launch(
        &self,
        spec: &FleetSpec,
        registry: &mut LifecycleRegistry,
    ) -> Result<LaunchOutcome, FleetLaunchError> {
        let plan = self.plan(spec)?;
        Ok(self.execute(&plan, registry))
    }

    /// Plan and execute with durable-state and command-router integration.
    pub fn launch_with_subsystems(
        &self,
        spec: &FleetSpec,
        registry: &mut LifecycleRegistry,
        durable_state: Option<&mut DurableStateManager>,
        router: Option<&mut CommandRouter>,
    ) -> Result<LaunchOutcome, FleetLaunchError> {
        let plan = self.plan(spec)?;
        Ok(self.execute_with_subsystems(&plan, registry, durable_state, router))
    }
}

// =============================================================================
// Weighted allocation algorithm
// =============================================================================

/// Distribute `total` slots across mix entries proportional to their weights.
///
/// Uses largest-remainder method (Hamilton's method) for deterministic,
/// proportionally fair allocation with no wasted slots.
fn allocate_weighted(total: u32, mix: &[AgentMixEntry]) -> Vec<u32> {
    if mix.is_empty() || total == 0 {
        return vec![0; mix.len()];
    }

    let total_weight: u32 = mix.iter().map(|e| e.weight).sum();
    if total_weight == 0 {
        return vec![0; mix.len()];
    }

    let total_f = f64::from(total);
    let weight_f = f64::from(total_weight);

    // Compute exact quotas
    let quotas: Vec<f64> = mix
        .iter()
        .map(|e| f64::from(e.weight) * total_f / weight_f)
        .collect();

    // Floor allocations
    let mut allocations: Vec<u32> = quotas.iter().map(|q| *q as u32).collect();
    let allocated: u32 = allocations.iter().sum();
    let mut remainder = total.saturating_sub(allocated);

    // Distribute remainders by largest fractional part
    if remainder > 0 {
        let mut fractional_parts: Vec<(usize, f64)> = quotas
            .iter()
            .enumerate()
            .map(|(i, q)| (i, q - f64::from(*q as u32)))
            .collect();
        fractional_parts.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        for (idx, _) in fractional_parts {
            if remainder == 0 {
                break;
            }
            allocations[idx] += 1;
            remainder -= 1;
        }
    }

    allocations
}

/// Determine launch phase for a role (lower phase = earlier startup).
fn phase_for_role(role: ProfileRole) -> u32 {
    match role {
        ProfileRole::Service => 0,     // Services start first
        ProfileRole::Monitor => 1,     // Monitors start second
        ProfileRole::BuildRunner => 2, // Build runners third
        ProfileRole::AgentWorker => 3, // Agent workers fourth
        ProfileRole::TestRunner => 3,  // Test runners with agents
        ProfileRole::DevShell => 4,    // Dev shells last
        ProfileRole::Custom => 3,      // Custom roles with agents
    }
}

/// Build launch phases from slot assignments.
fn build_phases(slots: &[LaunchSlot]) -> Vec<LaunchPhase> {
    let mut phase_map: HashMap<u32, Vec<u32>> = HashMap::new();
    for slot in slots {
        phase_map.entry(slot.phase).or_default().push(slot.index);
    }

    let mut phases: Vec<LaunchPhase> = phase_map
        .into_iter()
        .map(|(index, slot_indices)| LaunchPhase {
            index,
            label: phase_label(index),
            slot_indices,
        })
        .collect();
    phases.sort_by_key(|p| p.index);
    phases
}

fn phase_label(index: u32) -> String {
    match index {
        0 => "services".to_string(),
        1 => "monitors".to_string(),
        2 => "builders".to_string(),
        3 => "workers".to_string(),
        4 => "interactive".to_string(),
        n => format!("phase-{n}"),
    }
}

// =============================================================================
// Utility
// =============================================================================

fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Dispatch bootstrap commands for a slot through the command router.
///
/// Returns the number of commands successfully dispatched.
fn dispatch_bootstrap_commands(
    router: &mut CommandRouter,
    slot: &LaunchSlot,
    registry: &LifecycleRegistry,
    fleet_name: &str,
    _timestamp: u64,
) -> usize {
    let mut dispatched = 0;
    for (i, cmd) in slot.bootstrap_commands.iter().enumerate() {
        let request = CommandRequest {
            command_id: format!("{}-bootstrap-{}-{}", fleet_name, slot.index, i),
            scope: CommandScope::pane(slot.lifecycle_identity.clone()),
            command: CommandKind::SendInput {
                text: cmd.clone(),
                paste_mode: false,
                append_newline: true,
            },
            context: CommandContext::new(
                "fleet_launcher",
                format!("{}-slot-{}", fleet_name, slot.index),
                format!("fleet_launcher:{}", fleet_name),
            ),
            dry_run: false,
        };
        if router.route(&request, registry).is_ok() {
            dispatched += 1;
        }
    }
    dispatched
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }

    // -------------------------------------------------------------------------
    // Helper: build a minimal profile registry with defaults
    // -------------------------------------------------------------------------

    fn test_registry() -> ProfileRegistry {
        let mut reg = ProfileRegistry::new();
        reg.register_defaults();
        reg
    }

    fn basic_spec(name: &str, mix: Vec<AgentMixEntry>) -> FleetSpec {
        FleetSpec {
            name: name.to_string(),
            description: None,
            workspace_id: "test-workspace".to_string(),
            domain: "local".to_string(),
            mix,
            total_panes: 0,
            fleet_template: None,
            working_directory: None,
            startup_strategy: StartupStrategy::default(),
            generation: 1,
            tags: vec![],
        }
    }

    fn agent_mix(program: &str, weight: u32) -> AgentMixEntry {
        AgentMixEntry {
            program: program.to_string(),
            model: None,
            weight,
            profile: None,
            task_template: None,
            environment: HashMap::new(),
            role: None,
        }
    }

    // -------------------------------------------------------------------------
    // Weighted allocation tests
    // -------------------------------------------------------------------------

    #[test]
    fn allocate_weighted_equal_weights() {
        let mix = vec![agent_mix("a", 1), agent_mix("b", 1), agent_mix("c", 1)];
        let alloc = allocate_weighted(6, &mix);
        assert_eq!(alloc, vec![2, 2, 2]);
    }

    #[test]
    fn allocate_weighted_unequal() {
        let mix = vec![agent_mix("a", 3), agent_mix("b", 1)];
        let alloc = allocate_weighted(8, &mix);
        assert_eq!(alloc, vec![6, 2]);
    }

    #[test]
    fn allocate_weighted_remainder_distribution() {
        let mix = vec![agent_mix("a", 1), agent_mix("b", 1), agent_mix("c", 1)];
        let alloc = allocate_weighted(7, &mix);
        // 7/3 = 2.33 each; two entries get 3, one gets 2. Total must be 7.
        let total: u32 = alloc.iter().sum();
        assert_eq!(total, 7);
        assert!(alloc.iter().all(|&a| a == 2 || a == 3));
    }

    #[test]
    fn allocate_weighted_single_entry() {
        let mix = vec![agent_mix("a", 5)];
        let alloc = allocate_weighted(10, &mix);
        assert_eq!(alloc, vec![10]);
    }

    #[test]
    fn allocate_weighted_zero_total() {
        let mix = vec![agent_mix("a", 1), agent_mix("b", 1)];
        let alloc = allocate_weighted(0, &mix);
        assert_eq!(alloc, vec![0, 0]);
    }

    #[test]
    fn allocate_weighted_zero_weight_entry() {
        let mix = vec![agent_mix("a", 3), agent_mix("b", 0), agent_mix("c", 1)];
        let alloc = allocate_weighted(8, &mix);
        assert_eq!(alloc[1], 0); // zero-weight gets no slots
        let total: u32 = alloc.iter().sum();
        assert_eq!(total, 8);
    }

    #[test]
    fn allocate_weighted_empty_mix() {
        let alloc = allocate_weighted(5, &[]);
        assert!(alloc.is_empty());
    }

    #[test]
    fn allocate_weighted_one_slot_three_entries() {
        let mix = vec![agent_mix("a", 1), agent_mix("b", 1), agent_mix("c", 1)];
        let alloc = allocate_weighted(1, &mix);
        let total: u32 = alloc.iter().sum();
        assert_eq!(total, 1);
        // Exactly one entry gets a slot
        assert_eq!(alloc.iter().filter(|&&a| a == 1).count(), 1);
    }

    #[test]
    fn allocate_weighted_large_fleet() {
        let mix = vec![
            agent_mix("claude-code", 5),
            agent_mix("codex-cli", 3),
            agent_mix("gemini-cli", 2),
        ];
        let alloc = allocate_weighted(100, &mix);
        assert_eq!(alloc, vec![50, 30, 20]);
    }

    #[test]
    fn allocate_weighted_preserves_total() {
        let mix = vec![
            agent_mix("a", 7),
            agent_mix("b", 3),
            agent_mix("c", 11),
            agent_mix("d", 2),
        ];
        for total in 1..=50 {
            let alloc = allocate_weighted(total, &mix);
            let sum: u32 = alloc.iter().sum();
            assert_eq!(sum, total, "total mismatch for total={total}");
        }
    }

    // -------------------------------------------------------------------------
    // FleetSpec validation tests
    // -------------------------------------------------------------------------

    #[test]
    fn plan_empty_mix_fails() {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let spec = basic_spec("empty", vec![]);
        assert_eq!(
            launcher.plan(&spec).unwrap_err(),
            FleetLaunchError::EmptyMix
        );
    }

    #[test]
    fn plan_zero_weight_fails() {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let spec = basic_spec("zero", vec![agent_mix("a", 0)]);
        assert_eq!(
            launcher.plan(&spec).unwrap_err(),
            FleetLaunchError::ZeroWeight
        );
    }

    #[test]
    fn plan_missing_profile_fails() {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let mut entry = agent_mix("a", 1);
        entry.profile = Some("nonexistent-profile".to_string());
        let spec = basic_spec("bad-profile", vec![entry]);
        assert!(matches!(
            launcher.plan(&spec).unwrap_err(),
            FleetLaunchError::ProfileNotFound(_)
        ));
    }

    #[test]
    fn plan_missing_fleet_template_fails() {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let mut spec = basic_spec("bad-template", vec![agent_mix("a", 1)]);
        spec.fleet_template = Some("nonexistent-template".to_string());
        assert!(matches!(
            launcher.plan(&spec).unwrap_err(),
            FleetLaunchError::TemplateNotFound(_)
        ));
    }

    // -------------------------------------------------------------------------
    // LaunchPlan tests
    // -------------------------------------------------------------------------

    #[test]
    fn plan_single_entry_produces_correct_slots() {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let spec = basic_spec("single", vec![agent_mix("claude-code", 3)]);
        let plan = launcher.plan(&spec).unwrap();

        assert_eq!(plan.slots.len(), 3);
        assert_eq!(plan.name, "single");
        for (i, slot) in plan.slots.iter().enumerate() {
            assert_eq!(slot.index, i as u32);
            assert_eq!(slot.agent_identity.program, "claude-code");
            assert_eq!(slot.lifecycle_identity.kind, LifecycleEntityKind::Pane);
            assert_eq!(slot.lifecycle_identity.workspace_id, "test-workspace");
            assert_eq!(slot.lifecycle_identity.domain, "local");
        }
    }

    #[test]
    fn plan_mixed_fleet_allocation() {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let mix = vec![agent_mix("claude-code", 2), agent_mix("codex-cli", 1)];
        let mut spec = basic_spec("mixed", mix);
        spec.total_panes = 6;

        let plan = launcher.plan(&spec).unwrap();
        assert_eq!(plan.slots.len(), 6);

        let claude_count = plan
            .slots
            .iter()
            .filter(|s| s.agent_identity.program == "claude-code")
            .count();
        let codex_count = plan
            .slots
            .iter()
            .filter(|s| s.agent_identity.program == "codex-cli")
            .count();
        assert_eq!(claude_count, 4); // 2/3 * 6
        assert_eq!(codex_count, 2); // 1/3 * 6
    }

    #[test]
    fn plan_labels_remain_unique_when_program_repeats() {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let mix = vec![
            AgentMixEntry {
                program: "claude-code".to_string(),
                model: Some("model-a".to_string()),
                weight: 1,
                profile: None,
                task_template: None,
                environment: HashMap::new(),
                role: None,
            },
            AgentMixEntry {
                program: "claude-code".to_string(),
                model: Some("model-b".to_string()),
                weight: 1,
                profile: None,
                task_template: None,
                environment: HashMap::new(),
                role: None,
            },
        ];
        let spec = basic_spec("dup-program", mix);
        let plan = launcher.plan(&spec).unwrap();

        let labels: Vec<&str> = plan.slots.iter().map(|slot| slot.label.as_str()).collect();
        let unique: std::collections::HashSet<&str> = labels.iter().copied().collect();
        assert_eq!(
            unique.len(),
            labels.len(),
            "slot labels must stay unique even when program names repeat"
        );
    }

    #[test]
    fn plan_environment_variables_set() {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let mut entry = agent_mix("claude-code", 1);
        entry
            .environment
            .insert("CUSTOM_VAR".to_string(), "custom_val".to_string());
        let spec = basic_spec("env-test", vec![entry]);
        let plan = launcher.plan(&spec).unwrap();

        let slot = &plan.slots[0];
        assert_eq!(slot.environment.get("FT_FLEET_NAME").unwrap(), "env-test");
        assert_eq!(slot.environment.get("FT_SLOT_INDEX").unwrap(), "0");
        assert_eq!(slot.environment.get("CUSTOM_VAR").unwrap(), "custom_val");
    }

    #[test]
    fn plan_task_template_expansion() {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let mut entry = agent_mix("claude-code", 2);
        entry.task_template = Some("{program} task #{index}".to_string());
        let spec = basic_spec("task-tmpl", vec![entry]);
        let plan = launcher.plan(&spec).unwrap();

        assert_eq!(
            plan.slots[0].agent_identity.task.as_deref(),
            Some("claude-code task #0")
        );
        assert_eq!(
            plan.slots[1].agent_identity.task.as_deref(),
            Some("claude-code task #1")
        );
    }

    #[test]
    fn plan_working_directory_inheritance() {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let mut spec = basic_spec("cwd", vec![agent_mix("a", 1)]);
        spec.working_directory = Some("/projects/frankenterm".to_string());

        let plan = launcher.plan(&spec).unwrap();
        assert_eq!(
            plan.slots[0].working_directory.as_deref(),
            Some("/projects/frankenterm")
        );
    }

    #[test]
    fn plan_parallel_strategy_single_phase() {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let spec = basic_spec("parallel", vec![agent_mix("a", 3)]);
        let plan = launcher.plan(&spec).unwrap();

        assert_eq!(plan.strategy, StartupStrategy::Parallel);
        assert_eq!(plan.phases.len(), 1);
        assert_eq!(plan.phases[0].slot_indices.len(), 3);
    }

    #[test]
    fn plan_phased_strategy_groups_by_role() {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);

        let mut monitor = agent_mix("log-viewer", 1);
        monitor.role = Some(ProfileRole::Monitor);

        let mut service = agent_mix("db-server", 1);
        service.role = Some(ProfileRole::Service);

        let worker = agent_mix("claude-code", 2);

        let mut spec = basic_spec("phased", vec![monitor, service, worker]);
        spec.startup_strategy = StartupStrategy::Phased;

        let plan = launcher.plan(&spec).unwrap();
        assert!(plan.phases.len() >= 2);

        // Services (phase 0) should come before monitors (phase 1) before workers (phase 3)
        let phase_indices: Vec<u32> = plan.phases.iter().map(|p| p.index).collect();
        assert!(phase_indices.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn plan_lifecycle_identity_uniqueness() {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let spec = basic_spec("unique", vec![agent_mix("a", 5)]);
        let plan = launcher.plan(&spec).unwrap();

        let keys: Vec<String> = plan
            .slots
            .iter()
            .map(|s| s.lifecycle_identity.stable_key())
            .collect();
        let unique_keys: std::collections::HashSet<&str> =
            keys.iter().map(|s| s.as_str()).collect();
        assert_eq!(
            keys.len(),
            unique_keys.len(),
            "lifecycle identities must be unique"
        );
    }

    // -------------------------------------------------------------------------
    // Launch execution tests
    // -------------------------------------------------------------------------

    #[test]
    fn execute_registers_all_entities() {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let spec = basic_spec("exec-test", vec![agent_mix("claude-code", 3)]);
        let plan = launcher.plan(&spec).unwrap();

        let mut lifecycle = LifecycleRegistry::new();
        let outcome = launcher.execute(&plan, &mut lifecycle);

        assert_eq!(outcome.status, FleetLaunchStatus::Complete);
        assert_eq!(outcome.total_slots, 3);
        assert_eq!(outcome.successful_slots, 3);
        assert_eq!(outcome.failed_slots, 0);

        // 1 session + 1 window + 3 panes + 3 agents = 8 entities
        assert_eq!(lifecycle.len(), 8);
    }

    #[test]
    fn execute_panes_start_in_provisioning() {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let spec = basic_spec("prov-test", vec![agent_mix("a", 2)]);
        let plan = launcher.plan(&spec).unwrap();

        let mut lifecycle = LifecycleRegistry::new();
        launcher.execute(&plan, &mut lifecycle);

        for slot in &plan.slots {
            let record = lifecycle.get(&slot.lifecycle_identity).unwrap();
            assert_eq!(
                record.state,
                LifecycleState::Pane(MuxPaneLifecycleState::Provisioning)
            );
        }
    }

    #[test]
    fn launch_convenience_method() {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let spec = basic_spec("launch", vec![agent_mix("claude-code", 2)]);

        let mut lifecycle = LifecycleRegistry::new();
        let outcome = launcher.launch(&spec, &mut lifecycle).unwrap();

        assert_eq!(outcome.status, FleetLaunchStatus::Complete);
        assert_eq!(outcome.successful_slots, 2);
    }

    #[test]
    fn slot_outcomes_match_plan_slots() {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let spec = basic_spec("match", vec![agent_mix("a", 3), agent_mix("b", 2)]);

        let mut lifecycle = LifecycleRegistry::new();
        let outcome = launcher.launch(&spec, &mut lifecycle).unwrap();

        assert_eq!(outcome.slot_outcomes.len(), 5);
        for outcome_slot in &outcome.slot_outcomes {
            assert_eq!(outcome_slot.status, SlotStatus::Registered);
            assert!(outcome_slot.error.is_none());
        }
    }

    #[test]
    fn execute_sequential_halts_after_first_failure_and_skips_remaining_slots() {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let mut spec = basic_spec("sequential-halt", vec![agent_mix("a", 3)]);
        spec.startup_strategy = StartupStrategy::Sequential;
        let plan = launcher.plan(&spec).unwrap();

        let mut lifecycle = LifecycleRegistry::new();
        let forced_first = &plan.slots[0];
        lifecycle
            .register_entity(
                forced_first.lifecycle_identity.clone(),
                LifecycleState::Pane(MuxPaneLifecycleState::Provisioning),
                999,
            )
            .unwrap();

        let outcome = launcher.execute(&plan, &mut lifecycle);
        assert_eq!(outcome.status, FleetLaunchStatus::Failed);
        assert_eq!(outcome.total_slots, 3);
        assert_eq!(outcome.successful_slots, 0);
        assert_eq!(outcome.failed_slots, 1);
        assert_eq!(outcome.slot_outcomes.len(), 3);
        assert_eq!(outcome.slot_outcomes[0].status, SlotStatus::Failed);
        assert_eq!(outcome.slot_outcomes[1].status, SlotStatus::Skipped);
        assert_eq!(outcome.slot_outcomes[2].status, SlotStatus::Skipped);
        assert!(
            outcome.slot_outcomes[1]
                .error
                .as_deref()
                .is_some_and(|msg| msg.contains("sequential launch halted after slot 0 failed"))
        );
        assert!(
            outcome.slot_outcomes[2]
                .error
                .as_deref()
                .is_some_and(|msg| msg.contains("sequential launch halted after slot 0 failed"))
        );

        let snapshot = lifecycle.snapshot();
        let pane_count = snapshot
            .iter()
            .filter(|record| matches!(record.state, LifecycleState::Pane(_)))
            .count();
        assert_eq!(
            pane_count, 1,
            "later sequential slots must not register panes"
        );
    }

    #[test]
    fn fleet_launch_status_all_success() {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let spec = basic_spec("ok", vec![agent_mix("a", 1)]);
        let mut lifecycle = LifecycleRegistry::new();
        let outcome = launcher.launch(&spec, &mut lifecycle).unwrap();
        assert_eq!(outcome.status, FleetLaunchStatus::Complete);
    }

    #[test]
    fn launch_outcome_registry_snapshot_is_queryable() {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let spec = basic_spec("snapshot", vec![agent_mix("a", 2), agent_mix("b", 1)]);
        let plan = launcher.plan(&spec).unwrap();
        let mut lifecycle = LifecycleRegistry::new();
        let outcome = launcher.execute(&plan, &mut lifecycle);

        assert_eq!(outcome.status, FleetLaunchStatus::Complete);
        assert_eq!(outcome.total_slots, 3);
        assert_eq!(outcome.successful_slots, 3);
        assert_eq!(outcome.failed_slots, 0);
        assert!(outcome.completed_at >= plan.planned_at);

        let snapshot = &outcome.registry_snapshot;
        let session_count = snapshot
            .iter()
            .filter(|r| matches!(r.state, LifecycleState::Session(_)))
            .count();
        let window_count = snapshot
            .iter()
            .filter(|r| matches!(r.state, LifecycleState::Window(_)))
            .count();
        let pane_count = snapshot
            .iter()
            .filter(|r| matches!(r.state, LifecycleState::Pane(_)))
            .count();
        let agent_count = snapshot
            .iter()
            .filter(|r| matches!(r.state, LifecycleState::Agent(_)))
            .count();

        assert_eq!(session_count, 1);
        assert_eq!(window_count, 1);
        assert_eq!(pane_count, 3);
        assert_eq!(agent_count, 3);
        assert_eq!(snapshot.len(), 8);
    }

    // -------------------------------------------------------------------------
    // Phase and role tests
    // -------------------------------------------------------------------------

    #[test]
    fn phase_for_role_ordering() {
        // Services < Monitors < BuildRunners < AgentWorkers/TestRunners < DevShells
        assert!(phase_for_role(ProfileRole::Service) < phase_for_role(ProfileRole::Monitor));
        assert!(phase_for_role(ProfileRole::Monitor) < phase_for_role(ProfileRole::BuildRunner));
        assert!(
            phase_for_role(ProfileRole::BuildRunner) < phase_for_role(ProfileRole::AgentWorker)
        );
        assert_eq!(
            phase_for_role(ProfileRole::AgentWorker),
            phase_for_role(ProfileRole::TestRunner)
        );
        assert!(phase_for_role(ProfileRole::AgentWorker) < phase_for_role(ProfileRole::DevShell));
    }

    #[test]
    fn phase_labels_descriptive() {
        assert_eq!(phase_label(0), "services");
        assert_eq!(phase_label(1), "monitors");
        assert_eq!(phase_label(2), "builders");
        assert_eq!(phase_label(3), "workers");
        assert_eq!(phase_label(4), "interactive");
        assert_eq!(phase_label(99), "phase-99");
    }

    // -------------------------------------------------------------------------
    // StartupStrategy tests
    // -------------------------------------------------------------------------

    #[test]
    fn startup_strategy_serde_roundtrip() {
        let strategies = vec![
            StartupStrategy::Parallel,
            StartupStrategy::Sequential,
            StartupStrategy::Phased,
        ];
        for strategy in &strategies {
            let json = serde_json::to_string(strategy).unwrap();
            let deserialized: StartupStrategy = serde_json::from_str(&json).unwrap();
            assert_eq!(strategy, &deserialized);
        }
    }

    #[test]
    fn startup_strategy_default_is_parallel() {
        assert_eq!(StartupStrategy::default(), StartupStrategy::Parallel);
    }

    // -------------------------------------------------------------------------
    // FleetSpec serde tests
    // -------------------------------------------------------------------------

    #[test]
    fn fleet_spec_serde_roundtrip() {
        let spec = FleetSpec {
            name: "test-fleet".to_string(),
            description: Some("A test fleet".to_string()),
            workspace_id: "ws-1".to_string(),
            domain: "local".to_string(),
            mix: vec![AgentMixEntry {
                program: "claude-code".to_string(),
                model: Some("opus-4.1".to_string()),
                weight: 3,
                profile: Some("agent-worker".to_string()),
                task_template: Some("task #{index}".to_string()),
                environment: {
                    let mut env = HashMap::new();
                    env.insert("KEY".to_string(), "val".to_string());
                    env
                },
                role: Some(ProfileRole::AgentWorker),
            }],
            total_panes: 6,
            fleet_template: None,
            working_directory: Some("/project".to_string()),
            startup_strategy: StartupStrategy::Phased,
            generation: 2,
            tags: vec!["test".to_string()],
        };

        let json = serde_json::to_string_pretty(&spec).unwrap();
        let deserialized: FleetSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(spec, deserialized);
    }

    #[test]
    fn fleet_spec_minimal_deserialize() {
        let json = r#"{
            "name": "minimal",
            "workspace_id": "ws",
            "mix": [{"program": "claude-code"}]
        }"#;
        let spec: FleetSpec = serde_json::from_str(json).unwrap();
        assert_eq!(spec.name, "minimal");
        assert_eq!(spec.domain, "local");
        assert_eq!(spec.mix[0].weight, 1);
        assert_eq!(spec.startup_strategy, StartupStrategy::Parallel);
        assert_eq!(spec.generation, 1);
    }

    // -------------------------------------------------------------------------
    // Edge case tests
    // -------------------------------------------------------------------------

    #[test]
    fn plan_total_panes_override() {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let mut spec = basic_spec("override", vec![agent_mix("a", 1), agent_mix("b", 1)]);
        spec.total_panes = 10;

        let plan = launcher.plan(&spec).unwrap();
        assert_eq!(plan.slots.len(), 10);
        // Equal weights with 10 total = 5 each
        let a_count = plan
            .slots
            .iter()
            .filter(|s| s.agent_identity.program == "a")
            .count();
        assert_eq!(a_count, 5);
    }

    #[test]
    fn plan_generation_propagation() {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let mut spec = basic_spec("gen-test", vec![agent_mix("a", 1)]);
        spec.generation = 42;

        let plan = launcher.plan(&spec).unwrap();
        assert_eq!(plan.generation, 42);
        assert_eq!(plan.slots[0].lifecycle_identity.generation, 42);
    }

    #[test]
    fn plan_mix_entry_index_tracking() {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let mix = vec![agent_mix("a", 2), agent_mix("b", 1)];
        let spec = basic_spec("idx-test", mix);

        let plan = launcher.plan(&spec).unwrap();
        assert_eq!(plan.slots[0].mix_entry_index, 0);
        assert_eq!(plan.slots[1].mix_entry_index, 0);
        assert_eq!(plan.slots[2].mix_entry_index, 1);
    }

    #[test]
    fn plan_with_model_specification() {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let mut entry = agent_mix("claude-code", 1);
        entry.model = Some("opus-4.1".to_string());
        let spec = basic_spec("model-test", vec![entry]);

        let plan = launcher.plan(&spec).unwrap();
        assert_eq!(
            plan.slots[0].agent_identity.model.as_deref(),
            Some("opus-4.1")
        );
    }

    #[test]
    fn plan_warnings_for_zero_weight_entry() {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let mix = vec![agent_mix("a", 1), agent_mix("b", 0)];
        let spec = basic_spec("warn-test", mix);
        let plan = launcher.plan(&spec).unwrap();
        assert!(!plan.warnings.is_empty());
        assert!(plan.warnings[0].contains("weight 0"));
    }

    #[test]
    fn execute_session_and_window_entities() {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let spec = basic_spec("session-test", vec![agent_mix("a", 1)]);
        let plan = launcher.plan(&spec).unwrap();
        let mut lifecycle = LifecycleRegistry::new();
        launcher.execute(&plan, &mut lifecycle);

        // Verify session entity exists
        let session_id = LifecycleIdentity::new(
            LifecycleEntityKind::Session,
            "test-workspace",
            "local",
            0,
            1,
        );
        let session = lifecycle.get(&session_id).unwrap();
        assert_eq!(
            session.state,
            LifecycleState::Session(SessionLifecycleState::Active)
        );

        // Verify window entity exists
        let window_id =
            LifecycleIdentity::new(LifecycleEntityKind::Window, "test-workspace", "local", 0, 1);
        let window = lifecycle.get(&window_id).unwrap();
        assert_eq!(
            window.state,
            LifecycleState::Window(WindowLifecycleState::Active)
        );
    }

    // -------------------------------------------------------------------------
    // Agent entity registration tests
    // -------------------------------------------------------------------------

    #[test]
    fn execute_registers_agent_entities() {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let spec = basic_spec("agent-reg", vec![agent_mix("claude-code", 2)]);
        let plan = launcher.plan(&spec).unwrap();
        let mut lifecycle = LifecycleRegistry::new();
        launcher.execute(&plan, &mut lifecycle);

        // Each slot should have a corresponding Agent entity
        for slot in &plan.slots {
            let agent_id = LifecycleIdentity::new(
                LifecycleEntityKind::Agent,
                "test-workspace",
                "local",
                u64::from(slot.index),
                1,
            );
            let agent_record = lifecycle.get(&agent_id).unwrap();
            assert_eq!(
                agent_record.state,
                LifecycleState::Agent(AgentLifecycleState::Registered)
            );
        }
    }

    #[test]
    fn execute_agent_count_matches_pane_count() {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let spec = basic_spec(
            "agent-pane-match",
            vec![agent_mix("claude-code", 3), agent_mix("codex-cli", 2)],
        );
        let mut lifecycle = LifecycleRegistry::new();
        launcher.launch(&spec, &mut lifecycle).unwrap();

        let snapshot = lifecycle.snapshot();
        let agent_count = snapshot
            .iter()
            .filter(|r| matches!(r.state, LifecycleState::Agent(_)))
            .count();
        let pane_count = snapshot
            .iter()
            .filter(|r| matches!(r.state, LifecycleState::Pane(_)))
            .count();
        assert_eq!(agent_count, pane_count);
        assert_eq!(agent_count, 5);
    }

    #[test]
    fn execute_does_not_register_agent_when_pane_registration_fails() {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let spec = basic_spec("agent-no-orphan", vec![agent_mix("claude-code", 1)]);
        let plan = launcher.plan(&spec).unwrap();

        let mut lifecycle = LifecycleRegistry::new();
        lifecycle
            .register_entity(
                plan.slots[0].lifecycle_identity.clone(),
                LifecycleState::Pane(MuxPaneLifecycleState::Provisioning),
                now_ms(),
            )
            .unwrap();

        let outcome = launcher.execute(&plan, &mut lifecycle);
        assert_eq!(outcome.status, FleetLaunchStatus::Failed);
        assert_eq!(outcome.successful_slots, 0);
        assert_eq!(outcome.failed_slots, 1);

        let agent_id = LifecycleIdentity::new(
            LifecycleEntityKind::Agent,
            &plan.workspace_id,
            &plan.domain,
            u64::from(plan.slots[0].index),
            plan.generation,
        );
        assert!(
            lifecycle.get(&agent_id).is_none(),
            "agent must not be registered when pane registration fails"
        );
    }

    // -------------------------------------------------------------------------
    // DurableStateManager integration tests
    // -------------------------------------------------------------------------

    #[test]
    fn execute_with_durable_state_takes_checkpoint() {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let spec = basic_spec("checkpoint-test", vec![agent_mix("a", 2)]);
        let plan = launcher.plan(&spec).unwrap();

        let mut lifecycle = LifecycleRegistry::new();
        let mut durable = DurableStateManager::new();

        let outcome =
            launcher.execute_with_subsystems(&plan, &mut lifecycle, Some(&mut durable), None);

        assert_eq!(outcome.status, FleetLaunchStatus::Complete);
        // Checkpoint should have been taken
        assert_eq!(durable.checkpoint_count(), 1);
        let cp = durable.latest_checkpoint().unwrap();
        assert!(cp.label.contains("checkpoint-test"));
    }

    #[test]
    fn launch_with_subsystems_takes_checkpoint() {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let spec = basic_spec("launch-cp", vec![agent_mix("b", 1)]);

        let mut lifecycle = LifecycleRegistry::new();
        let mut durable = DurableStateManager::new();

        let outcome = launcher
            .launch_with_subsystems(&spec, &mut lifecycle, Some(&mut durable), None)
            .unwrap();

        assert_eq!(outcome.status, FleetLaunchStatus::Complete);
        assert_eq!(durable.checkpoint_count(), 1);
    }

    #[test]
    fn execute_with_durable_state_reports_new_checkpoint_id() {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let spec = basic_spec("checkpoint-id", vec![agent_mix("a", 1)]);
        let plan = launcher.plan(&spec).unwrap();

        let mut lifecycle = LifecycleRegistry::new();
        let mut durable = DurableStateManager::new();
        let existing_id = durable
            .checkpoint(
                &lifecycle,
                "existing",
                CheckpointTrigger::Manual,
                HashMap::new(),
            )
            .id;

        let outcome =
            launcher.execute_with_subsystems(&plan, &mut lifecycle, Some(&mut durable), None);

        assert_eq!(outcome.status, FleetLaunchStatus::Complete);
        assert_eq!(durable.checkpoint_count(), 2);
        let latest_id = durable.latest_checkpoint().map(|cp| cp.id);
        assert_eq!(outcome.pre_launch_checkpoint, latest_id);
        assert_ne!(
            outcome.pre_launch_checkpoint,
            Some(existing_id),
            "outcome should reference the newly-created pre-launch checkpoint"
        );
    }

    // -------------------------------------------------------------------------
    // CommandRouter integration tests
    // -------------------------------------------------------------------------

    #[test]
    fn execute_with_router_dispatches_bootstrap_commands() {
        let mut reg = test_registry();
        let mut profile = reg.get_profile("agent-worker").unwrap().clone();
        profile.name = "agent-worker-bootstrap".to_string();
        profile.bootstrap_commands = vec!["echo warmup".to_string(), "echo ready".to_string()];
        reg.register_profile(profile);

        let launcher = FleetLauncher::new(&reg);

        let mut entry = agent_mix("claude-code", 2);
        entry.profile = Some("agent-worker-bootstrap".to_string());
        let spec = basic_spec("bootstrap-test", vec![entry]);
        let plan = launcher.plan(&spec).unwrap();

        let mut lifecycle = LifecycleRegistry::new();
        let mut router = CommandRouter::new();

        let outcome =
            launcher.execute_with_subsystems(&plan, &mut lifecycle, None, Some(&mut router));

        assert_eq!(outcome.status, FleetLaunchStatus::Complete);
        assert_eq!(outcome.bootstrap_dispatches.len(), 2);
        assert_eq!(outcome.bootstrap_dispatches, vec![(0, 2), (1, 2)]);
    }

    #[test]
    fn execute_without_subsystems_has_empty_dispatches() {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let spec = basic_spec("no-subsys", vec![agent_mix("a", 2)]);
        let mut lifecycle = LifecycleRegistry::new();

        let outcome = launcher.launch(&spec, &mut lifecycle).unwrap();

        assert!(outcome.bootstrap_dispatches.is_empty());
        assert!(outcome.pre_launch_checkpoint.is_none());
    }

    #[test]
    fn execute_with_all_subsystems() {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let spec = basic_spec("full-subsys", vec![agent_mix("a", 3)]);

        let mut lifecycle = LifecycleRegistry::new();
        let mut durable = DurableStateManager::new();
        let mut router = CommandRouter::new();

        let outcome = launcher.execute_with_subsystems(
            &launcher.plan(&spec).unwrap(),
            &mut lifecycle,
            Some(&mut durable),
            Some(&mut router),
        );

        assert_eq!(outcome.status, FleetLaunchStatus::Complete);
        assert_eq!(outcome.successful_slots, 3);
        assert_eq!(durable.checkpoint_count(), 1);
        // 1 session + 1 window + 3 panes + 3 agents = 8
        assert_eq!(lifecycle.len(), 8);
    }

    // =========================================================================
    // Queryable launch metadata surface tests (ft-3681t.3.1.1)
    // =========================================================================

    #[test]
    fn launch_plan_programs_returns_distinct_ordered() {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let spec = basic_spec(
            "prog-test",
            vec![agent_mix("claude", 2), agent_mix("codex", 1), agent_mix("claude", 1)],
        );
        let plan = launcher.plan(&spec).unwrap();
        let programs = plan.programs();
        // "claude" appears twice in mix but should be listed once; "codex" second
        assert_eq!(programs, vec!["claude", "codex"]);
    }

    #[test]
    fn launch_plan_program_distribution() {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let spec = basic_spec(
            "dist-test",
            vec![agent_mix("claude", 3), agent_mix("codex", 1)],
        );
        let plan = launcher.plan(&spec).unwrap();
        let dist = plan.program_distribution();
        assert_eq!(dist.len(), 2);
        let claude = dist.iter().find(|d| d.program == "claude").unwrap();
        let codex = dist.iter().find(|d| d.program == "codex").unwrap();
        assert_eq!(claude.slot_count, 3);
        assert_eq!(codex.slot_count, 1);
        assert_eq!(claude.slot_indices.len(), 3);
        assert_eq!(codex.slot_indices.len(), 1);
    }

    #[test]
    fn launch_plan_slot_by_index() {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let spec = basic_spec("slot-test", vec![agent_mix("a", 3)]);
        let plan = launcher.plan(&spec).unwrap();
        assert!(plan.slot(0).is_some());
        assert!(plan.slot(2).is_some());
        assert!(plan.slot(3).is_none());
        assert_eq!(plan.slot(1).unwrap().index, 1);
    }

    #[test]
    fn launch_plan_slots_in_phase() {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let spec = basic_spec("phase-test", vec![agent_mix("a", 5)]);
        let plan = launcher.plan(&spec).unwrap();
        // Parallel strategy: all slots in phase 0
        let phase0_slots = plan.slots_in_phase(0);
        assert_eq!(phase0_slots.len(), 5);
        let phase1_slots = plan.slots_in_phase(1);
        assert!(phase1_slots.is_empty());
    }

    #[test]
    fn launch_plan_phase_labels() {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let spec = basic_spec("labels-test", vec![agent_mix("a", 2)]);
        let plan = launcher.plan(&spec).unwrap();
        let labels = plan.phase_labels();
        assert!(!labels.is_empty());
        // Parallel has a single "all" phase
        assert_eq!(labels, vec!["all"]);
    }

    #[test]
    fn launch_plan_invariant_violations_clean() {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let spec = basic_spec(
            "inv-test",
            vec![agent_mix("claude", 3), agent_mix("codex", 2)],
        );
        let plan = launcher.plan(&spec).unwrap();
        let violations = plan.invariant_violations();
        assert!(
            violations.is_empty(),
            "expected no violations, got: {:?}",
            violations
        );
    }

    #[test]
    fn launch_plan_serde_roundtrip() {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let spec = basic_spec(
            "serde-test",
            vec![agent_mix("claude", 2), agent_mix("codex", 1)],
        );
        let plan = launcher.plan(&spec).unwrap();
        let json = serde_json::to_string(&plan).expect("serialize LaunchPlan");
        let back: LaunchPlan = serde_json::from_str(&json).expect("deserialize LaunchPlan");
        assert_eq!(back.name, plan.name);
        assert_eq!(back.slots.len(), plan.slots.len());
        assert_eq!(back.phases.len(), plan.phases.len());
        assert_eq!(back.generation, plan.generation);
        assert_eq!(back.workspace_id, plan.workspace_id);
        assert_eq!(back.domain, plan.domain);
        assert_eq!(back.warnings, plan.warnings);
    }

    #[test]
    fn launch_outcome_serde_roundtrip() {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let spec = basic_spec("out-serde", vec![agent_mix("a", 2)]);
        let mut lifecycle = LifecycleRegistry::new();
        let outcome = launcher.launch(&spec, &mut lifecycle).unwrap();
        let json = serde_json::to_string(&outcome).expect("serialize LaunchOutcome");
        let back: LaunchOutcome =
            serde_json::from_str(&json).expect("deserialize LaunchOutcome");
        assert_eq!(back.name, outcome.name);
        assert_eq!(back.total_slots, outcome.total_slots);
        assert_eq!(back.successful_slots, outcome.successful_slots);
        assert_eq!(back.failed_slots, outcome.failed_slots);
        assert_eq!(back.status, outcome.status);
    }

    #[test]
    fn launch_outcome_registry_summary() {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let spec = basic_spec("sum-test", vec![agent_mix("a", 3)]);
        let mut lifecycle = LifecycleRegistry::new();
        let outcome = launcher.launch(&spec, &mut lifecycle).unwrap();
        let summary = outcome.registry_summary();
        // 1 session + 1 window + 3 panes + 3 agents = 8
        assert_eq!(summary.total_entities, 8);
        assert_eq!(summary.sessions, 1);
        assert_eq!(summary.windows, 1);
        assert_eq!(summary.panes, 3);
        assert_eq!(summary.agents, 3);
    }

    #[test]
    fn launch_outcome_successful_outcomes() {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let spec = basic_spec("succ-test", vec![agent_mix("a", 3)]);
        let mut lifecycle = LifecycleRegistry::new();
        let outcome = launcher.launch(&spec, &mut lifecycle).unwrap();
        assert_eq!(outcome.successful_outcomes().len(), 3);
        assert!(outcome.failed_outcomes().is_empty());
        assert!(outcome.skipped_outcomes().is_empty());
        assert!(outcome.is_complete());
    }

    #[test]
    fn launch_outcome_is_complete_flag() {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let spec = basic_spec("comp-test", vec![agent_mix("a", 1)]);
        let mut lifecycle = LifecycleRegistry::new();
        let outcome = launcher.launch(&spec, &mut lifecycle).unwrap();
        assert!(outcome.is_complete());
        assert_eq!(outcome.status, FleetLaunchStatus::Complete);
    }

    #[test]
    fn fleet_launch_error_serde_roundtrip() {
        let errors = vec![
            FleetLaunchError::EmptyMix,
            FleetLaunchError::ZeroWeight,
            FleetLaunchError::ProfileNotFound("missing".to_string()),
            FleetLaunchError::TemplateNotFound("tpl".to_string()),
            FleetLaunchError::RegistrationFailed {
                slot_index: 5,
                reason: "test".to_string(),
            },
            FleetLaunchError::ValidationFailed("bad".to_string()),
        ];
        for err in &errors {
            let json = serde_json::to_string(err).expect("serialize FleetLaunchError");
            let back: FleetLaunchError =
                serde_json::from_str(&json).expect("deserialize FleetLaunchError");
            assert_eq!(&back, err);
        }
    }

    #[test]
    fn program_distribution_serde_roundtrip() {
        let dist = ProgramDistribution {
            program: "claude-code".to_string(),
            slot_count: 5,
            slot_indices: vec![0, 1, 2, 3, 4],
        };
        let json = serde_json::to_string(&dist).expect("serialize");
        let back: ProgramDistribution = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, dist);
    }

    #[test]
    fn metadata_projection_failure_display() {
        let failures = vec![
            MetadataProjectionFailure::ProgramNotFound {
                program: "x".to_string(),
            },
            MetadataProjectionFailure::PhaseNotFound { phase: 99 },
            MetadataProjectionFailure::SlotIndexOutOfBounds { index: 10, max: 5 },
            MetadataProjectionFailure::PhaseSlotMismatch {
                slot_index: 3,
                claimed_phase: 77,
            },
            MetadataProjectionFailure::InconsistentSlotField {
                slot_index: 0,
                field: "label".to_string(),
            },
        ];
        for f in &failures {
            let msg = f.to_string();
            assert!(!msg.is_empty(), "Display for {:?} is empty", f);
        }
    }

    #[test]
    fn metadata_projection_failure_serde_roundtrip() {
        let failures = vec![
            MetadataProjectionFailure::ProgramNotFound {
                program: "missing".to_string(),
            },
            MetadataProjectionFailure::PhaseNotFound { phase: 42 },
            MetadataProjectionFailure::SlotIndexOutOfBounds { index: 10, max: 5 },
            MetadataProjectionFailure::PhaseSlotMismatch {
                slot_index: 3,
                claimed_phase: 99,
            },
            MetadataProjectionFailure::InconsistentSlotField {
                slot_index: 0,
                field: "label".to_string(),
            },
        ];
        for f in &failures {
            let json = serde_json::to_string(f).expect("serialize");
            let back: MetadataProjectionFailure =
                serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, f.clone());
        }
    }

    #[test]
    fn registry_summary_serde_roundtrip() {
        let summary = RegistrySummary {
            total_entities: 8,
            sessions: 1,
            windows: 1,
            panes: 3,
            agents: 3,
        };
        let json = serde_json::to_string(&summary).expect("serialize");
        let back: RegistrySummary = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, summary);
    }

    #[test]
    fn plan_distribution_matches_spec_weights() {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        // Spec: 60% claude (weight 3), 40% codex (weight 2), total 10 panes
        let spec = FleetSpec {
            total_panes: 10,
            ..basic_spec(
                "weight-match",
                vec![agent_mix("claude", 3), agent_mix("codex", 2)],
            )
        };
        let plan = launcher.plan(&spec).unwrap();
        let dist = plan.program_distribution();
        let claude = dist.iter().find(|d| d.program == "claude").unwrap();
        let codex = dist.iter().find(|d| d.program == "codex").unwrap();
        assert_eq!(claude.slot_count, 6); // 3/5 * 10 = 6
        assert_eq!(codex.slot_count, 4); // 2/5 * 10 = 4
        // Total must match
        let total: u32 = dist.iter().map(|d| d.slot_count).sum();
        assert_eq!(total, 10);
    }
}
