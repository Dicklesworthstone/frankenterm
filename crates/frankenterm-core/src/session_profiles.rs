// =============================================================================
// Session profile/template/persona engine (ft-3681t.2.4)
//
// Declarative spawn profiles for fleet setup: role defaults, command bootstraps,
// environment setup, resource hints, and policy posture. Makes fleet provisioning
// reproducible and codified rather than ad-hoc.
// =============================================================================

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

// =============================================================================
// Session profile
// =============================================================================

/// A declarative profile for spawning sessions/panes with consistent configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionProfile {
    /// Unique profile name (e.g., "agent-worker", "monitor", "dev-shell").
    pub name: String,
    /// Human-readable description.
    #[serde(default)]
    pub description: Option<String>,
    /// Role classification for this profile.
    pub role: ProfileRole,
    /// Command to execute when spawning a pane with this profile.
    #[serde(default)]
    pub spawn_command: Option<SpawnCommand>,
    /// Environment variables to set.
    #[serde(default)]
    pub environment: HashMap<String, String>,
    /// Working directory for spawned panes.
    #[serde(default)]
    pub working_directory: Option<String>,
    /// Resource budget hints for the spawned entity.
    #[serde(default)]
    pub resource_hints: ResourceHints,
    /// Policy posture for this profile.
    #[serde(default)]
    pub policy: ProfilePolicy,
    /// Layout template to apply when this profile creates a window.
    #[serde(default)]
    pub layout_template: Option<String>,
    /// Bootstrap commands to run after spawn.
    #[serde(default)]
    pub bootstrap_commands: Vec<String>,
    /// Tags for filtering and grouping.
    #[serde(default)]
    pub tags: Vec<String>,
    /// When this profile was last modified (epoch ms).
    #[serde(default)]
    pub updated_at: u64,
}

/// Role classification for a session profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileRole {
    /// Interactive development shell.
    DevShell,
    /// AI agent worker pane.
    AgentWorker,
    /// Monitoring/log viewer pane.
    Monitor,
    /// Build/CI runner pane.
    BuildRunner,
    /// Test execution pane.
    TestRunner,
    /// Server/service pane.
    Service,
    /// Custom role with a name.
    Custom,
}

/// Command specification for spawning panes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpawnCommand {
    /// The command to execute (shell-expanded).
    pub command: String,
    /// Arguments to pass to the command.
    #[serde(default)]
    pub args: Vec<String>,
    /// If true, use the user's default shell to execute the command.
    #[serde(default = "default_true")]
    pub use_shell: bool,
}

fn default_true() -> bool {
    true
}

/// Resource budget hints for pane provisioning.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResourceHints {
    /// Minimum rows for pane sizing.
    #[serde(default = "default_min_rows")]
    pub min_rows: u16,
    /// Minimum columns for pane sizing.
    #[serde(default = "default_min_cols")]
    pub min_cols: u16,
    /// Preferred rows (best-effort).
    #[serde(default)]
    pub preferred_rows: Option<u16>,
    /// Preferred columns (best-effort).
    #[serde(default)]
    pub preferred_cols: Option<u16>,
    /// Maximum scrollback lines.
    #[serde(default = "default_scrollback")]
    pub max_scrollback: u32,
    /// Priority weight for resource allocation (higher = more resources).
    #[serde(default = "default_weight")]
    pub priority_weight: u32,
}

impl Default for ResourceHints {
    fn default() -> Self {
        Self {
            min_rows: default_min_rows(),
            min_cols: default_min_cols(),
            preferred_rows: None,
            preferred_cols: None,
            max_scrollback: default_scrollback(),
            priority_weight: default_weight(),
        }
    }
}

fn default_min_rows() -> u16 {
    10
}
fn default_min_cols() -> u16 {
    40
}
fn default_scrollback() -> u32 {
    10_000
}
fn default_weight() -> u32 {
    1
}

/// Policy posture for a profile.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProfilePolicy {
    /// Whether this profile allows sending input.
    #[serde(default = "default_true")]
    pub allow_input: bool,
    /// Whether this profile allows capturing output.
    #[serde(default = "default_true")]
    pub allow_capture: bool,
    /// Whether this profile can be interrupted.
    #[serde(default = "default_true")]
    pub allow_interrupt: bool,
    /// Whether this profile's panes can be closed by automation.
    #[serde(default = "default_true")]
    pub allow_auto_close: bool,
    /// Whether to enable command auditing for this profile.
    #[serde(default)]
    pub audit_commands: bool,
    /// Maximum idle time before the pane is eligible for draining (seconds).
    /// 0 = no idle limit.
    #[serde(default)]
    pub idle_timeout_secs: u64,
}

impl Default for ProfilePolicy {
    fn default() -> Self {
        Self {
            allow_input: true,
            allow_capture: true,
            allow_interrupt: true,
            allow_auto_close: true,
            audit_commands: false,
            idle_timeout_secs: 0,
        }
    }
}

// =============================================================================
// Persona (pre-composed profile + environment bundle)
// =============================================================================

/// A persona bundles a profile with additional identity/context for agent roles.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Persona {
    /// Persona name (e.g., "builder-agent", "monitor-fleet-alpha").
    pub name: String,
    /// The session profile to use for this persona.
    pub profile_name: String,
    /// Additional environment overrides specific to this persona.
    #[serde(default)]
    pub env_overrides: HashMap<String, String>,
    /// Agent-specific identity metadata.
    #[serde(default)]
    pub agent_identity: Option<AgentIdentitySpec>,
    /// Description of what this persona does.
    #[serde(default)]
    pub description: Option<String>,
}

/// Agent identity specification for a persona.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentIdentitySpec {
    /// Agent program name (e.g., "claude-code", "codex-cli").
    pub program: String,
    /// Agent model (e.g., "opus-4.1", "gpt5-codex").
    #[serde(default)]
    pub model: Option<String>,
    /// Task assignment for the agent.
    #[serde(default)]
    pub task: Option<String>,
}

// =============================================================================
// Fleet template
// =============================================================================

/// A fleet template describes a complete multi-pane setup with assigned personas.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FleetTemplate {
    /// Template name (e.g., "dev-swarm-4", "monitoring-grid").
    pub name: String,
    /// Human-readable description.
    #[serde(default)]
    pub description: Option<String>,
    /// Pane slots with assigned personas.
    pub slots: Vec<FleetSlot>,
    /// Layout template to use for arranging the panes.
    #[serde(default)]
    pub layout_template: Option<String>,
    /// Startup orchestration mode for this fleet.
    #[serde(default)]
    pub startup_strategy: FleetStartupStrategy,
    /// Optional topology profile for deterministic native mux initialization.
    ///
    /// If unset, `layout_template` acts as the fallback topology initializer.
    #[serde(default)]
    pub topology_profile: Option<String>,
    /// Optional weighted target mix by agent program (e.g. codex-cli/claude-code).
    #[serde(default)]
    pub program_mix_targets: Vec<FleetProgramTarget>,
}

/// Startup orchestration strategy for a fleet.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum FleetStartupStrategy {
    /// Launch pane slots in deterministic phase order. Slots in the same phase
    /// may be launched in parallel by downstream executors.
    #[default]
    Phased,
    /// Launch pane slots one-by-one in deterministic order.
    Serial,
}

/// Weighted target for a specific agent program in a fleet.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FleetProgramTarget {
    pub program: String,
    #[serde(default = "default_weight")]
    pub weight: u32,
}

/// A single slot in a fleet template.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FleetSlot {
    /// Slot label for identification.
    pub label: String,
    /// Persona to assign to this slot (or profile name directly).
    #[serde(default)]
    pub persona: Option<String>,
    /// Direct profile name (used if persona is not set).
    #[serde(default)]
    pub profile: Option<String>,
    /// Additional environment for this specific slot.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Relative launch weight for this slot (higher = earlier within a phase).
    #[serde(default = "default_weight")]
    pub weight: u32,
    /// Startup phase; lower phases launch before higher phases.
    #[serde(default)]
    pub startup_phase: u16,
}

// =============================================================================
// Profile registry
// =============================================================================

/// Registry of session profiles, personas, and fleet templates.
#[derive(Debug, Clone, Default)]
pub struct ProfileRegistry {
    profiles: HashMap<String, SessionProfile>,
    personas: HashMap<String, Persona>,
    fleet_templates: HashMap<String, FleetTemplate>,
}

impl ProfileRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    // -- Profiles --

    /// Register a session profile. Overwrites if name exists.
    pub fn register_profile(&mut self, profile: SessionProfile) {
        self.profiles.insert(profile.name.clone(), profile);
    }

    /// Look up a profile by name.
    pub fn get_profile(&self, name: &str) -> Option<&SessionProfile> {
        self.profiles.get(name)
    }

    /// List all profile names.
    pub fn profile_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.profiles.keys().map(|s| s.as_str()).collect();
        names.sort_unstable();
        names
    }

    /// Number of registered profiles.
    pub fn profile_count(&self) -> usize {
        self.profiles.len()
    }

    // -- Personas --

    /// Register a persona. Overwrites if name exists.
    pub fn register_persona(&mut self, persona: Persona) {
        self.personas.insert(persona.name.clone(), persona);
    }

    /// Look up a persona by name.
    pub fn get_persona(&self, name: &str) -> Option<&Persona> {
        self.personas.get(name)
    }

    /// List all persona names.
    pub fn persona_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.personas.keys().map(|s| s.as_str()).collect();
        names.sort_unstable();
        names
    }

    // -- Fleet templates --

    /// Register a fleet template.
    pub fn register_fleet_template(&mut self, template: FleetTemplate) {
        self.fleet_templates.insert(template.name.clone(), template);
    }

    /// Look up a fleet template by name.
    pub fn get_fleet_template(&self, name: &str) -> Option<&FleetTemplate> {
        self.fleet_templates.get(name)
    }

    /// List all fleet template names.
    pub fn fleet_template_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.fleet_templates.keys().map(|s| s.as_str()).collect();
        names.sort_unstable();
        names
    }

    // -- Defaults --

    /// Register built-in default profiles.
    pub fn register_defaults(&mut self) {
        self.register_profile(SessionProfile {
            name: "dev-shell".into(),
            description: Some("Interactive development shell".into()),
            role: ProfileRole::DevShell,
            spawn_command: None, // use default shell
            environment: HashMap::new(),
            working_directory: None,
            resource_hints: ResourceHints {
                min_rows: 24,
                min_cols: 80,
                preferred_rows: Some(40),
                preferred_cols: Some(120),
                max_scrollback: 50_000,
                priority_weight: 2,
            },
            policy: ProfilePolicy {
                allow_input: true,
                allow_capture: true,
                allow_interrupt: true,
                allow_auto_close: false,
                audit_commands: false,
                idle_timeout_secs: 0,
            },
            layout_template: None,
            bootstrap_commands: vec![],
            tags: vec!["interactive".into(), "dev".into()],
            updated_at: epoch_ms(),
        });

        self.register_profile(SessionProfile {
            name: "agent-worker".into(),
            description: Some("AI agent worker pane".into()),
            role: ProfileRole::AgentWorker,
            spawn_command: None,
            environment: HashMap::new(),
            working_directory: None,
            resource_hints: ResourceHints {
                min_rows: 24,
                min_cols: 80,
                preferred_rows: None,
                preferred_cols: None,
                max_scrollback: 10_000,
                priority_weight: 1,
            },
            policy: ProfilePolicy {
                allow_input: true,
                allow_capture: true,
                allow_interrupt: true,
                allow_auto_close: true,
                audit_commands: true,
                idle_timeout_secs: 3600,
            },
            layout_template: None,
            bootstrap_commands: vec![],
            tags: vec!["agent".into(), "automated".into()],
            updated_at: epoch_ms(),
        });

        self.register_profile(SessionProfile {
            name: "monitor".into(),
            description: Some("Monitoring/log viewer pane".into()),
            role: ProfileRole::Monitor,
            spawn_command: None,
            environment: HashMap::new(),
            working_directory: None,
            resource_hints: ResourceHints {
                min_rows: 10,
                min_cols: 80,
                preferred_rows: Some(20),
                preferred_cols: None,
                max_scrollback: 100_000,
                priority_weight: 1,
            },
            policy: ProfilePolicy {
                allow_input: false,
                allow_capture: true,
                allow_interrupt: false,
                allow_auto_close: true,
                audit_commands: false,
                idle_timeout_secs: 0,
            },
            layout_template: None,
            bootstrap_commands: vec![],
            tags: vec!["monitor".into(), "readonly".into()],
            updated_at: epoch_ms(),
        });

        self.register_profile(SessionProfile {
            name: "build-runner".into(),
            description: Some("Build/CI runner pane".into()),
            role: ProfileRole::BuildRunner,
            spawn_command: None,
            environment: HashMap::new(),
            working_directory: None,
            resource_hints: ResourceHints {
                min_rows: 20,
                min_cols: 100,
                preferred_rows: None,
                preferred_cols: None,
                max_scrollback: 50_000,
                priority_weight: 3,
            },
            policy: ProfilePolicy {
                allow_input: true,
                allow_capture: true,
                allow_interrupt: true,
                allow_auto_close: true,
                audit_commands: true,
                idle_timeout_secs: 7200,
            },
            layout_template: None,
            bootstrap_commands: vec![],
            tags: vec!["build".into(), "ci".into()],
            updated_at: epoch_ms(),
        });
    }

    // -- Resolve --

    /// Resolve a persona to a fully-merged profile with all overrides applied.
    pub fn resolve_persona(&self, persona_name: &str) -> Option<ResolvedProfile> {
        let persona = self.personas.get(persona_name)?;
        let base_profile = self.profiles.get(&persona.profile_name)?;

        let mut env = base_profile.environment.clone();
        for (k, v) in &persona.env_overrides {
            env.insert(k.clone(), v.clone());
        }

        Some(ResolvedProfile {
            profile: base_profile.clone(),
            environment: env,
            agent_identity: persona.agent_identity.clone(),
            persona_name: persona_name.to_string(),
        })
    }

    /// Resolve a fleet template to a list of resolved pane specifications.
    pub fn resolve_fleet(&self, template_name: &str) -> Option<ResolvedFleet> {
        let fleet = self.fleet_templates.get(template_name)?;

        let mut panes = Vec::new();
        for slot in &fleet.slots {
            let resolved = if let Some(persona_name) = &slot.persona {
                self.resolve_persona(persona_name)
            } else if let Some(profile_name) = &slot.profile {
                self.get_profile(profile_name).map(|p| ResolvedProfile {
                    profile: p.clone(),
                    environment: p.environment.clone(),
                    agent_identity: None,
                    persona_name: profile_name.clone(),
                })
            } else {
                continue;
            };

            if let Some(mut resolved) = resolved {
                // Apply slot-level env overrides
                for (k, v) in &slot.env {
                    resolved.environment.insert(k.clone(), v.clone());
                }
                panes.push(ResolvedFleetPane {
                    label: slot.label.clone(),
                    resolved,
                    weight: slot.weight.max(1),
                    startup_phase: slot.startup_phase,
                    launch_order: 0,
                });
            }
        }

        let launch_plan = FleetLaunchPlan::from_resolved(
            &fleet.name,
            fleet.startup_strategy,
            fleet.topology_profile.as_ref(),
            fleet.layout_template.as_ref(),
            &fleet.program_mix_targets,
            &mut panes,
        );

        Some(ResolvedFleet {
            name: fleet.name.clone(),
            layout_template: fleet.layout_template.clone(),
            startup_strategy: fleet.startup_strategy,
            topology_profile: fleet.topology_profile.clone(),
            panes,
            launch_plan,
        })
    }
}

/// A fully-resolved profile with all overrides applied.
#[derive(Debug, Clone)]
pub struct ResolvedProfile {
    pub profile: SessionProfile,
    pub environment: HashMap<String, String>,
    pub agent_identity: Option<AgentIdentitySpec>,
    pub persona_name: String,
}

/// A resolved fleet with all pane specifications.
#[derive(Debug, Clone)]
pub struct ResolvedFleet {
    pub name: String,
    pub layout_template: Option<String>,
    pub startup_strategy: FleetStartupStrategy,
    pub topology_profile: Option<String>,
    pub panes: Vec<ResolvedFleetPane>,
    pub launch_plan: FleetLaunchPlan,
}

/// A single resolved pane in a fleet.
#[derive(Debug, Clone)]
pub struct ResolvedFleetPane {
    pub label: String,
    pub resolved: ResolvedProfile,
    pub weight: u32,
    pub startup_phase: u16,
    pub launch_order: usize,
}

/// Deterministic launch metadata derived from a resolved fleet.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FleetLaunchPlan {
    /// Fleet name this plan was derived from.
    pub fleet: String,
    /// Startup strategy selected by the fleet template.
    pub startup_strategy: FleetStartupStrategy,
    /// Topology initializer for deterministic native mux arrangement.
    pub topology_initializer: Option<String>,
    /// Slot labels in deterministic launch order.
    pub deterministic_order: Vec<String>,
    /// Startup phases in execution order with deterministic per-phase ordering.
    pub phases: Vec<FleetLaunchPhase>,
    /// Per-slot launch metadata in deterministic order.
    pub slot_metadata: Vec<FleetLaunchSlotMetadata>,
    /// Aggregate launch weight across all slots.
    pub total_weight: u32,
    /// Weighted program mix summary (queryable by downstream schedulers).
    pub program_mix: Vec<FleetProgramMix>,
    /// Weighted target-vs-actual deltas for each program in the union of
    /// configured targets and resolved fleet panes.
    pub program_mix_deltas: Vec<FleetProgramMixDelta>,
}

/// Deterministic startup phase group.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FleetLaunchPhase {
    pub phase: u16,
    pub slots: Vec<String>,
}

/// Per-slot launch metadata for downstream scheduler queries.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FleetLaunchSlotMetadata {
    /// Slot label.
    pub label: String,
    /// Startup phase this slot belongs to.
    pub startup_phase: u16,
    /// Deterministic launch order index (0-based).
    pub launch_order: usize,
    /// Weighted launch priority contribution.
    pub weight: u32,
    /// Program associated with this slot (or `shell` fallback).
    pub program: String,
    /// Resolved persona identifier for this slot.
    pub persona: String,
    /// Resolved profile identifier for this slot.
    pub profile: String,
}

/// Weighted program distribution summary for the fleet.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FleetProgramMix {
    pub program: String,
    pub slot_count: u32,
    pub total_weight: u32,
    pub slots: Vec<String>,
}

/// Delta between configured weighted targets and resolved weighted mix.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FleetProgramMixDelta {
    pub program: String,
    pub target_weight: u32,
    pub actual_weight: u32,
    pub actual_slots: u32,
    pub weight_delta: i64,
}

/// Invariant violation for launch-plan metadata projection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FleetLaunchInvariantViolation {
    /// Stable reason code for downstream diagnostics.
    pub reason_code: String,
    /// Human-readable detail for triage.
    pub detail: String,
}

impl FleetLaunchPlan {
    fn from_resolved(
        fleet: &str,
        startup_strategy: FleetStartupStrategy,
        topology_profile: Option<&String>,
        layout_template: Option<&String>,
        program_mix_targets: &[FleetProgramTarget],
        panes: &mut [ResolvedFleetPane],
    ) -> Self {
        // Deterministic global ordering:
        // 1) startup phase asc
        // 2) weight desc (heavier slots first)
        // 3) label asc
        // 4) stable index asc
        let mut indices: Vec<usize> = (0..panes.len()).collect();
        indices.sort_by(|&a, &b| {
            panes[a]
                .startup_phase
                .cmp(&panes[b].startup_phase)
                .then_with(|| panes[b].weight.cmp(&panes[a].weight))
                .then_with(|| panes[a].label.cmp(&panes[b].label))
                .then_with(|| a.cmp(&b))
        });

        for (position, idx) in indices.iter().copied().enumerate() {
            panes[idx].launch_order = position;
        }

        let slot_metadata = indices
            .iter()
            .map(|idx| {
                let pane = &panes[*idx];
                FleetLaunchSlotMetadata {
                    label: pane.label.clone(),
                    startup_phase: pane.startup_phase,
                    launch_order: pane.launch_order,
                    weight: pane.weight,
                    program: Self::slot_program(pane),
                    persona: pane.resolved.persona_name.clone(),
                    profile: pane.resolved.profile.name.clone(),
                }
            })
            .collect::<Vec<_>>();
        let deterministic_order = slot_metadata
            .iter()
            .map(|slot| slot.label.clone())
            .collect::<Vec<_>>();

        let mut phase_map: BTreeMap<u16, Vec<String>> = BTreeMap::new();
        let mut mix_map: BTreeMap<String, FleetProgramMix> = BTreeMap::new();
        let mut total_weight = 0_u32;

        for slot in &slot_metadata {
            total_weight = total_weight.saturating_add(slot.weight);
            phase_map
                .entry(slot.startup_phase)
                .or_default()
                .push(slot.label.clone());

            let entry = mix_map
                .entry(slot.program.clone())
                .or_insert_with(|| FleetProgramMix {
                    program: slot.program.clone(),
                    slot_count: 0,
                    total_weight: 0,
                    slots: Vec::new(),
                });
            entry.slot_count = entry.slot_count.saturating_add(1);
            entry.total_weight = entry.total_weight.saturating_add(slot.weight);
            entry.slots.push(slot.label.clone());
        }

        let phases = phase_map
            .into_iter()
            .map(|(phase, slots)| FleetLaunchPhase { phase, slots })
            .collect::<Vec<_>>();

        let program_mix = mix_map.into_values().collect::<Vec<_>>();
        let program_mix_deltas = Self::build_program_mix_deltas(program_mix_targets, &program_mix);

        Self {
            fleet: fleet.to_string(),
            startup_strategy,
            topology_initializer: topology_profile
                .cloned()
                .or_else(|| layout_template.cloned()),
            deterministic_order,
            phases,
            slot_metadata,
            total_weight,
            program_mix,
            program_mix_deltas,
        }
    }

    fn slot_program(pane: &ResolvedFleetPane) -> String {
        pane.resolved
            .agent_identity
            .as_ref()
            .map(|identity| identity.program.clone())
            .unwrap_or_else(|| "shell".to_string())
    }

    fn build_program_mix_deltas(
        program_mix_targets: &[FleetProgramTarget],
        program_mix: &[FleetProgramMix],
    ) -> Vec<FleetProgramMixDelta> {
        let mut target_by_program: BTreeMap<String, u32> = BTreeMap::new();
        for target in program_mix_targets {
            let entry = target_by_program.entry(target.program.clone()).or_default();
            *entry = entry.saturating_add(target.weight);
        }

        let mut actual_by_program: BTreeMap<String, (u32, u32)> = BTreeMap::new();
        for mix in program_mix {
            actual_by_program
                .entry(mix.program.clone())
                .and_modify(|(slots, weight)| {
                    *slots = slots.saturating_add(mix.slot_count);
                    *weight = weight.saturating_add(mix.total_weight);
                })
                .or_insert((mix.slot_count, mix.total_weight));
        }

        let mut programs: BTreeMap<String, ()> = BTreeMap::new();
        for program in target_by_program.keys() {
            programs.insert(program.clone(), ());
        }
        for program in actual_by_program.keys() {
            programs.insert(program.clone(), ());
        }

        programs
            .into_keys()
            .map(|program| {
                let target_weight = target_by_program.get(&program).copied().unwrap_or(0);
                let (actual_slots, actual_weight) =
                    actual_by_program.get(&program).copied().unwrap_or((0, 0));
                FleetProgramMixDelta {
                    program,
                    target_weight,
                    actual_weight,
                    actual_slots,
                    weight_delta: i64::from(actual_weight) - i64::from(target_weight),
                }
            })
            .collect()
    }

    /// Look up a slot metadata record by label.
    pub fn slot(&self, label: &str) -> Option<&FleetLaunchSlotMetadata> {
        self.slot_metadata.iter().find(|slot| slot.label == label)
    }

    /// Return all slots for a startup phase in deterministic order.
    pub fn slots_for_phase(&self, phase: u16) -> Vec<&FleetLaunchSlotMetadata> {
        self.slot_metadata
            .iter()
            .filter(|slot| slot.startup_phase == phase)
            .collect()
    }

    /// Return the aggregate launch weight for a given program.
    pub fn program_weight(&self, program: &str) -> u32 {
        self.program_mix
            .iter()
            .find(|entry| entry.program == program)
            .map_or(0, |entry| entry.total_weight)
    }

    /// Validate deterministic launch-plan invariants and emit reason-coded violations.
    pub fn invariant_violations(&self) -> Vec<FleetLaunchInvariantViolation> {
        let mut violations = Vec::new();

        if self.deterministic_order.len() != self.slot_metadata.len() {
            violations.push(FleetLaunchInvariantViolation {
                reason_code: "fleet.launch_plan.slot_count_mismatch".to_string(),
                detail: format!(
                    "deterministic_order has {} entries but slot_metadata has {}",
                    self.deterministic_order.len(),
                    self.slot_metadata.len()
                ),
            });
        }

        let mut by_order = self.slot_metadata.iter().collect::<Vec<_>>();
        by_order.sort_by_key(|a| a.launch_order);
        for (expected_order, slot) in by_order.iter().enumerate() {
            if slot.launch_order != expected_order {
                violations.push(FleetLaunchInvariantViolation {
                    reason_code: "fleet.launch_plan.launch_order_non_contiguous".to_string(),
                    detail: format!(
                        "slot '{}' has launch_order {}, expected {}",
                        slot.label, slot.launch_order, expected_order
                    ),
                });
            }
        }

        let expected_order = by_order
            .iter()
            .map(|slot| slot.label.clone())
            .collect::<Vec<_>>();
        if expected_order != self.deterministic_order {
            violations.push(FleetLaunchInvariantViolation {
                reason_code: "fleet.launch_plan.deterministic_order_mismatch".to_string(),
                detail: format!(
                    "expected deterministic order {:?} but found {:?}",
                    expected_order, self.deterministic_order
                ),
            });
        }

        for phase in &self.phases {
            let expected_phase_slots = self
                .slots_for_phase(phase.phase)
                .into_iter()
                .map(|slot| slot.label.clone())
                .collect::<Vec<_>>();
            if expected_phase_slots != phase.slots {
                violations.push(FleetLaunchInvariantViolation {
                    reason_code: "fleet.launch_plan.phase_slots_mismatch".to_string(),
                    detail: format!(
                        "phase {} expected slots {:?} but found {:?}",
                        phase.phase, expected_phase_slots, phase.slots
                    ),
                });
            }

            for label in &phase.slots {
                if self.slot(label).is_none() {
                    violations.push(FleetLaunchInvariantViolation {
                        reason_code: "fleet.launch_plan.phase_slot_unknown".to_string(),
                        detail: format!(
                            "phase {} references unknown slot '{}'",
                            phase.phase, label
                        ),
                    });
                }
            }
        }

        let summed_weight = self.slot_metadata.iter().fold(0_u64, |acc, slot| {
            acc.saturating_add(u64::from(slot.weight))
        });
        if summed_weight > u64::from(u32::MAX) {
            violations.push(FleetLaunchInvariantViolation {
                reason_code: "fleet.launch_plan.weight_overflow".to_string(),
                detail: format!(
                    "summed slot weight {} exceeds u32::MAX ({})",
                    summed_weight,
                    u32::MAX
                ),
            });
        }
        let expected_total = summed_weight.min(u64::from(u32::MAX)) as u32;
        if self.total_weight != expected_total {
            violations.push(FleetLaunchInvariantViolation {
                reason_code: "fleet.launch_plan.total_weight_mismatch".to_string(),
                detail: format!(
                    "total_weight={}, expected {} from slot metadata",
                    self.total_weight, expected_total
                ),
            });
        }

        let mut expected_mix: BTreeMap<String, (u32, u32, Vec<String>)> = BTreeMap::new();
        for slot in &self.slot_metadata {
            let entry = expected_mix
                .entry(slot.program.clone())
                .or_insert_with(|| (0, 0, Vec::new()));
            entry.0 = entry.0.saturating_add(1);
            entry.1 = entry.1.saturating_add(slot.weight);
            entry.2.push(slot.label.clone());
        }
        let actual_mix = self
            .program_mix
            .iter()
            .map(|entry| {
                (
                    entry.program.clone(),
                    (entry.slot_count, entry.total_weight, entry.slots.clone()),
                )
            })
            .collect::<BTreeMap<_, _>>();
        if expected_mix != actual_mix {
            violations.push(FleetLaunchInvariantViolation {
                reason_code: "fleet.launch_plan.program_mix_mismatch".to_string(),
                detail: format!(
                    "program_mix {:?} does not match slot metadata aggregation {:?}",
                    self.program_mix, expected_mix
                ),
            });
        }

        for delta in &self.program_mix_deltas {
            let (expected_slots, expected_weight) = expected_mix
                .get(&delta.program)
                .map_or((0, 0), |(slots, weight, _)| (*slots, *weight));
            if delta.actual_slots != expected_slots || delta.actual_weight != expected_weight {
                violations.push(FleetLaunchInvariantViolation {
                    reason_code: "fleet.launch_plan.program_mix_delta_mismatch".to_string(),
                    detail: format!(
                        "program '{}' delta actual_slots/weight=({},{}) expected ({},{})",
                        delta.program,
                        delta.actual_slots,
                        delta.actual_weight,
                        expected_slots,
                        expected_weight
                    ),
                });
            }
        }

        violations
    }
}

// =============================================================================
// Validation
// =============================================================================

/// Errors from profile validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileValidationError {
    /// Profile name is empty.
    EmptyName,
    /// Persona references a non-existent profile.
    ProfileNotFound { persona: String, profile: String },
    /// Fleet slot references a non-existent persona/profile.
    SlotRefNotFound {
        fleet: String,
        slot: String,
        ref_name: String,
    },
    /// Fleet has no slots.
    EmptyFleet { name: String },
    /// Fleet has duplicate slot labels.
    DuplicateSlotLabel { fleet: String, label: String },
    /// Fleet slot weight must be non-zero.
    InvalidSlotWeight { fleet: String, slot: String },
    /// Program target entry has an empty program id.
    EmptyProgramMixTarget { fleet: String },
    /// Program target has invalid zero weight.
    InvalidProgramMixWeight { fleet: String, program: String },
    /// Fleet contains duplicate program targets.
    DuplicateProgramMixTarget { fleet: String, program: String },
    /// Bootstrap command is empty.
    EmptyBootstrapCommand { profile: String, index: usize },
}

impl std::fmt::Display for ProfileValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyName => write!(f, "profile name cannot be empty"),
            Self::ProfileNotFound { persona, profile } => {
                write!(
                    f,
                    "persona '{persona}' references unknown profile '{profile}'"
                )
            }
            Self::SlotRefNotFound {
                fleet,
                slot,
                ref_name,
            } => {
                write!(
                    f,
                    "fleet '{fleet}' slot '{slot}' references unknown '{ref_name}'"
                )
            }
            Self::EmptyFleet { name } => {
                write!(f, "fleet template '{name}' has no slots")
            }
            Self::DuplicateSlotLabel { fleet, label } => {
                write!(f, "fleet '{fleet}' contains duplicate slot label '{label}'")
            }
            Self::InvalidSlotWeight { fleet, slot } => {
                write!(f, "fleet '{fleet}' slot '{slot}' has invalid zero weight")
            }
            Self::EmptyProgramMixTarget { fleet } => {
                write!(
                    f,
                    "fleet '{fleet}' contains program mix target with empty program"
                )
            }
            Self::InvalidProgramMixWeight { fleet, program } => {
                write!(
                    f,
                    "fleet '{fleet}' program target '{program}' has invalid zero weight"
                )
            }
            Self::DuplicateProgramMixTarget { fleet, program } => {
                write!(
                    f,
                    "fleet '{fleet}' contains duplicate program target '{program}'"
                )
            }
            Self::EmptyBootstrapCommand { profile, index } => {
                write!(
                    f,
                    "profile '{profile}' has empty bootstrap command at index {index}"
                )
            }
        }
    }
}

impl ProfileRegistry {
    /// Validate all registered profiles, personas, and fleet templates.
    pub fn validate(&self) -> Vec<ProfileValidationError> {
        let mut errors = Vec::new();

        // Validate profiles
        for profile in self.profiles.values() {
            if profile.name.is_empty() {
                errors.push(ProfileValidationError::EmptyName);
            }
            for (i, cmd) in profile.bootstrap_commands.iter().enumerate() {
                if cmd.trim().is_empty() {
                    errors.push(ProfileValidationError::EmptyBootstrapCommand {
                        profile: profile.name.clone(),
                        index: i,
                    });
                }
            }
        }

        // Validate personas
        for persona in self.personas.values() {
            if !self.profiles.contains_key(&persona.profile_name) {
                errors.push(ProfileValidationError::ProfileNotFound {
                    persona: persona.name.clone(),
                    profile: persona.profile_name.clone(),
                });
            }
        }

        // Validate fleet templates
        for fleet in self.fleet_templates.values() {
            if fleet.slots.is_empty() {
                errors.push(ProfileValidationError::EmptyFleet {
                    name: fleet.name.clone(),
                });
            }
            let mut labels = HashSet::new();
            for slot in &fleet.slots {
                if !labels.insert(slot.label.clone()) {
                    errors.push(ProfileValidationError::DuplicateSlotLabel {
                        fleet: fleet.name.clone(),
                        label: slot.label.clone(),
                    });
                }
                if slot.weight == 0 {
                    errors.push(ProfileValidationError::InvalidSlotWeight {
                        fleet: fleet.name.clone(),
                        slot: slot.label.clone(),
                    });
                }
                if let Some(persona_name) = &slot.persona {
                    if !self.personas.contains_key(persona_name) {
                        errors.push(ProfileValidationError::SlotRefNotFound {
                            fleet: fleet.name.clone(),
                            slot: slot.label.clone(),
                            ref_name: persona_name.clone(),
                        });
                    }
                } else if let Some(profile_name) = &slot.profile {
                    if !self.profiles.contains_key(profile_name) {
                        errors.push(ProfileValidationError::SlotRefNotFound {
                            fleet: fleet.name.clone(),
                            slot: slot.label.clone(),
                            ref_name: profile_name.clone(),
                        });
                    }
                }
            }

            let mut target_programs = HashSet::new();
            for target in &fleet.program_mix_targets {
                let program = target.program.trim();
                if program.is_empty() {
                    errors.push(ProfileValidationError::EmptyProgramMixTarget {
                        fleet: fleet.name.clone(),
                    });
                    continue;
                }
                if !target_programs.insert(program.to_string()) {
                    errors.push(ProfileValidationError::DuplicateProgramMixTarget {
                        fleet: fleet.name.clone(),
                        program: program.to_string(),
                    });
                }
                if target.weight == 0 {
                    errors.push(ProfileValidationError::InvalidProgramMixWeight {
                        fleet: fleet.name.clone(),
                        program: program.to_string(),
                    });
                }
            }
        }

        errors
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

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -------------------------------------------------------------------------
    // SessionProfile tests
    // -------------------------------------------------------------------------

    #[test]
    fn profile_role_serde_roundtrip() {
        let roles = vec![
            ProfileRole::DevShell,
            ProfileRole::AgentWorker,
            ProfileRole::Monitor,
            ProfileRole::BuildRunner,
            ProfileRole::TestRunner,
            ProfileRole::Service,
            ProfileRole::Custom,
        ];

        for role in &roles {
            let json = serde_json::to_string(role).unwrap();
            let deserialized: ProfileRole = serde_json::from_str(&json).unwrap();
            assert_eq!(role, &deserialized);
        }
    }

    #[test]
    fn session_profile_serde_roundtrip() {
        let profile = SessionProfile {
            name: "test-profile".into(),
            description: Some("A test profile".into()),
            role: ProfileRole::AgentWorker,
            spawn_command: Some(SpawnCommand {
                command: "echo".into(),
                args: vec!["hello".into()],
                use_shell: true,
            }),
            environment: {
                let mut env = HashMap::new();
                env.insert("FOO".into(), "bar".into());
                env
            },
            working_directory: Some("/tmp".into()),
            resource_hints: ResourceHints::default(),
            policy: ProfilePolicy::default(),
            layout_template: Some("side-by-side".into()),
            bootstrap_commands: vec!["echo boot".into()],
            tags: vec!["test".into()],
            updated_at: 1234567890,
        };

        let json = serde_json::to_string_pretty(&profile).unwrap();
        let deserialized: SessionProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(profile, deserialized);
    }

    #[test]
    fn resource_hints_defaults() {
        let hints = ResourceHints::default();
        assert_eq!(hints.min_rows, 10);
        assert_eq!(hints.min_cols, 40);
        assert_eq!(hints.max_scrollback, 10_000);
        assert_eq!(hints.priority_weight, 1);
        assert!(hints.preferred_rows.is_none());
    }

    #[test]
    fn profile_policy_defaults() {
        let policy = ProfilePolicy::default();
        assert!(policy.allow_input);
        assert!(policy.allow_capture);
        assert!(policy.allow_interrupt);
        assert!(policy.allow_auto_close);
        assert!(!policy.audit_commands);
        assert_eq!(policy.idle_timeout_secs, 0);
    }

    // -------------------------------------------------------------------------
    // ProfileRegistry tests
    // -------------------------------------------------------------------------

    #[test]
    fn registry_defaults() {
        let mut reg = ProfileRegistry::new();
        assert_eq!(reg.profile_count(), 0);

        reg.register_defaults();
        assert!(reg.profile_count() >= 4);
        assert!(reg.get_profile("dev-shell").is_some());
        assert!(reg.get_profile("agent-worker").is_some());
        assert!(reg.get_profile("monitor").is_some());
        assert!(reg.get_profile("build-runner").is_some());
    }

    #[test]
    fn registry_default_dev_shell_no_auto_close() {
        let mut reg = ProfileRegistry::new();
        reg.register_defaults();

        let dev = reg.get_profile("dev-shell").unwrap();
        assert!(!dev.policy.allow_auto_close);
        assert_eq!(dev.role, ProfileRole::DevShell);
    }

    #[test]
    fn registry_default_agent_worker_audited() {
        let mut reg = ProfileRegistry::new();
        reg.register_defaults();

        let agent = reg.get_profile("agent-worker").unwrap();
        assert!(agent.policy.audit_commands);
        assert!(agent.policy.allow_auto_close);
        assert_eq!(agent.policy.idle_timeout_secs, 3600);
    }

    #[test]
    fn registry_default_monitor_readonly() {
        let mut reg = ProfileRegistry::new();
        reg.register_defaults();

        let mon = reg.get_profile("monitor").unwrap();
        assert!(!mon.policy.allow_input);
        assert!(!mon.policy.allow_interrupt);
        assert_eq!(mon.role, ProfileRole::Monitor);
    }

    #[test]
    fn registry_custom_profile() {
        let mut reg = ProfileRegistry::new();
        reg.register_profile(SessionProfile {
            name: "my-profile".into(),
            description: None,
            role: ProfileRole::Custom,
            spawn_command: None,
            environment: HashMap::new(),
            working_directory: None,
            resource_hints: ResourceHints::default(),
            policy: ProfilePolicy::default(),
            layout_template: None,
            bootstrap_commands: vec![],
            tags: vec![],
            updated_at: 0,
        });
        assert_eq!(reg.profile_names(), vec!["my-profile"]);
    }

    #[test]
    fn registry_overwrite_profile() {
        let mut reg = ProfileRegistry::new();
        reg.register_profile(SessionProfile {
            name: "x".into(),
            description: Some("v1".into()),
            role: ProfileRole::Custom,
            spawn_command: None,
            environment: HashMap::new(),
            working_directory: None,
            resource_hints: ResourceHints::default(),
            policy: ProfilePolicy::default(),
            layout_template: None,
            bootstrap_commands: vec![],
            tags: vec![],
            updated_at: 0,
        });
        reg.register_profile(SessionProfile {
            name: "x".into(),
            description: Some("v2".into()),
            role: ProfileRole::Custom,
            spawn_command: None,
            environment: HashMap::new(),
            working_directory: None,
            resource_hints: ResourceHints::default(),
            policy: ProfilePolicy::default(),
            layout_template: None,
            bootstrap_commands: vec![],
            tags: vec![],
            updated_at: 0,
        });
        assert_eq!(reg.profile_count(), 1);
        assert_eq!(
            reg.get_profile("x").unwrap().description.as_deref(),
            Some("v2")
        );
    }

    // -------------------------------------------------------------------------
    // Persona tests
    // -------------------------------------------------------------------------

    #[test]
    fn persona_serde_roundtrip() {
        let persona = Persona {
            name: "builder".into(),
            profile_name: "agent-worker".into(),
            env_overrides: {
                let mut m = HashMap::new();
                m.insert("ROLE".into(), "builder".into());
                m
            },
            agent_identity: Some(AgentIdentitySpec {
                program: "claude-code".into(),
                model: Some("opus-4.1".into()),
                task: Some("Build verification".into()),
            }),
            description: Some("Builder agent persona".into()),
        };

        let json = serde_json::to_string(&persona).unwrap();
        let deserialized: Persona = serde_json::from_str(&json).unwrap();
        assert_eq!(persona, deserialized);
    }

    #[test]
    fn resolve_persona() {
        let mut reg = ProfileRegistry::new();
        reg.register_defaults();

        reg.register_persona(Persona {
            name: "my-builder".into(),
            profile_name: "agent-worker".into(),
            env_overrides: {
                let mut m = HashMap::new();
                m.insert("AGENT_ROLE".into(), "builder".into());
                m
            },
            agent_identity: Some(AgentIdentitySpec {
                program: "claude-code".into(),
                model: None,
                task: None,
            }),
            description: None,
        });

        let resolved = reg.resolve_persona("my-builder").unwrap();
        assert_eq!(resolved.persona_name, "my-builder");
        assert_eq!(resolved.profile.name, "agent-worker");
        assert_eq!(
            resolved.environment.get("AGENT_ROLE").map(|s| s.as_str()),
            Some("builder")
        );
        assert!(resolved.agent_identity.is_some());
    }

    #[test]
    fn resolve_persona_not_found() {
        let reg = ProfileRegistry::new();
        assert!(reg.resolve_persona("nonexistent").is_none());
    }

    #[test]
    fn resolve_persona_missing_profile() {
        let mut reg = ProfileRegistry::new();
        reg.register_persona(Persona {
            name: "broken".into(),
            profile_name: "nonexistent".into(),
            env_overrides: HashMap::new(),
            agent_identity: None,
            description: None,
        });
        assert!(reg.resolve_persona("broken").is_none());
    }

    // -------------------------------------------------------------------------
    // Fleet template tests
    // -------------------------------------------------------------------------

    #[test]
    fn fleet_template_serde_roundtrip() {
        let fleet = FleetTemplate {
            name: "test-fleet".into(),
            description: Some("Test fleet".into()),
            slots: vec![
                FleetSlot {
                    label: "primary".into(),
                    persona: None,
                    profile: Some("dev-shell".into()),
                    env: HashMap::new(),
                    weight: 2,
                    startup_phase: 0,
                },
                FleetSlot {
                    label: "worker-1".into(),
                    persona: Some("builder".into()),
                    profile: None,
                    env: HashMap::new(),
                    weight: 1,
                    startup_phase: 1,
                },
            ],
            layout_template: Some("side-by-side".into()),
            startup_strategy: FleetStartupStrategy::Phased,
            topology_profile: Some("swarm-default".into()),
            program_mix_targets: vec![
                FleetProgramTarget {
                    program: "shell".into(),
                    weight: 2,
                },
                FleetProgramTarget {
                    program: "builder".into(),
                    weight: 1,
                },
            ],
        };

        let json = serde_json::to_string(&fleet).unwrap();
        let deserialized: FleetTemplate = serde_json::from_str(&json).unwrap();
        assert_eq!(fleet, deserialized);
    }

    #[test]
    fn resolve_fleet() {
        let mut reg = ProfileRegistry::new();
        reg.register_defaults();

        reg.register_persona(Persona {
            name: "worker-persona".into(),
            profile_name: "agent-worker".into(),
            env_overrides: HashMap::new(),
            agent_identity: Some(AgentIdentitySpec {
                program: "codex-cli".into(),
                model: Some("gpt-5-codex".into()),
                task: Some("worker".into()),
            }),
            description: None,
        });

        reg.register_fleet_template(FleetTemplate {
            name: "my-fleet".into(),
            description: None,
            slots: vec![
                FleetSlot {
                    label: "primary".into(),
                    persona: None,
                    profile: Some("dev-shell".into()),
                    env: HashMap::new(),
                    weight: 3,
                    startup_phase: 0,
                },
                FleetSlot {
                    label: "worker".into(),
                    persona: Some("worker-persona".into()),
                    profile: None,
                    env: {
                        let mut m = HashMap::new();
                        m.insert("SLOT_ID".into(), "w1".into());
                        m
                    },
                    weight: 1,
                    startup_phase: 1,
                },
            ],
            layout_template: Some("side-by-side".into()),
            startup_strategy: FleetStartupStrategy::Phased,
            topology_profile: Some("swarm-1+3".into()),
            program_mix_targets: vec![
                FleetProgramTarget {
                    program: "shell".into(),
                    weight: 3,
                },
                FleetProgramTarget {
                    program: "codex-cli".into(),
                    weight: 1,
                },
            ],
        });

        let fleet = reg.resolve_fleet("my-fleet").unwrap();
        assert_eq!(fleet.name, "my-fleet");
        assert_eq!(fleet.panes.len(), 2);
        assert_eq!(fleet.panes[0].label, "primary");
        assert_eq!(fleet.panes[0].resolved.profile.role, ProfileRole::DevShell);
        assert_eq!(fleet.panes[1].label, "worker");
        assert_eq!(
            fleet.panes[1]
                .resolved
                .environment
                .get("SLOT_ID")
                .map(|s| s.as_str()),
            Some("w1")
        );
        assert_eq!(fleet.panes[0].launch_order, 0);
        assert_eq!(fleet.panes[1].launch_order, 1);
        assert_eq!(
            fleet.launch_plan.deterministic_order,
            vec!["primary", "worker"]
        );
        assert_eq!(
            fleet.launch_plan.topology_initializer.as_deref(),
            Some("swarm-1+3")
        );
        assert_eq!(fleet.launch_plan.total_weight, 4);
        assert_eq!(fleet.launch_plan.program_mix.len(), 2);
        assert_eq!(fleet.launch_plan.program_mix[0].program, "codex-cli");
        assert_eq!(fleet.launch_plan.program_mix[0].slot_count, 1);
        assert_eq!(fleet.launch_plan.program_mix[0].total_weight, 1);
        assert_eq!(fleet.launch_plan.program_mix[1].program, "shell");
        assert_eq!(fleet.launch_plan.program_mix[1].slot_count, 1);
        assert_eq!(fleet.launch_plan.program_mix[1].total_weight, 3);
        assert_eq!(fleet.launch_plan.program_mix_deltas.len(), 2);
        assert_eq!(fleet.launch_plan.program_mix_deltas[0].program, "codex-cli");
        assert_eq!(fleet.launch_plan.program_mix_deltas[0].target_weight, 1);
        assert_eq!(fleet.launch_plan.program_mix_deltas[0].actual_weight, 1);
        assert_eq!(fleet.launch_plan.program_mix_deltas[0].weight_delta, 0);
        assert_eq!(fleet.launch_plan.program_mix_deltas[1].program, "shell");
        assert_eq!(fleet.launch_plan.program_mix_deltas[1].target_weight, 3);
        assert_eq!(fleet.launch_plan.program_mix_deltas[1].actual_weight, 3);
        assert_eq!(fleet.launch_plan.program_mix_deltas[1].weight_delta, 0);
        assert_eq!(fleet.launch_plan.slot_metadata.len(), 2);
        assert_eq!(fleet.launch_plan.slot_metadata[0].label, "primary");
        assert_eq!(fleet.launch_plan.slot_metadata[0].launch_order, 0);
        assert_eq!(fleet.launch_plan.slot_metadata[0].program, "shell");
        assert_eq!(fleet.launch_plan.slot_metadata[0].persona, "dev-shell");
        assert_eq!(fleet.launch_plan.slot_metadata[1].label, "worker");
        assert_eq!(fleet.launch_plan.slot_metadata[1].launch_order, 1);
        assert_eq!(fleet.launch_plan.slot_metadata[1].program, "codex-cli");
        assert_eq!(
            fleet
                .launch_plan
                .slot("worker")
                .map(|slot| slot.profile.as_str()),
            Some("agent-worker")
        );
        assert_eq!(
            fleet
                .launch_plan
                .slots_for_phase(1)
                .into_iter()
                .map(|slot| slot.label.as_str())
                .collect::<Vec<_>>(),
            vec!["worker"]
        );
        assert_eq!(fleet.launch_plan.program_weight("shell"), 3);
        assert_eq!(fleet.launch_plan.program_weight("codex-cli"), 1);
        assert!(fleet.launch_plan.invariant_violations().is_empty());
    }

    #[test]
    fn resolve_fleet_launch_order_deterministic_by_phase_weight_and_label() {
        let mut reg = ProfileRegistry::new();
        reg.register_defaults();
        reg.register_persona(Persona {
            name: "agent-a".into(),
            profile_name: "agent-worker".into(),
            env_overrides: HashMap::new(),
            agent_identity: Some(AgentIdentitySpec {
                program: "claude-code".into(),
                model: None,
                task: None,
            }),
            description: None,
        });
        reg.register_persona(Persona {
            name: "agent-b".into(),
            profile_name: "agent-worker".into(),
            env_overrides: HashMap::new(),
            agent_identity: Some(AgentIdentitySpec {
                program: "codex-cli".into(),
                model: None,
                task: None,
            }),
            description: None,
        });

        reg.register_fleet_template(FleetTemplate {
            name: "ordering".into(),
            description: None,
            slots: vec![
                FleetSlot {
                    label: "zeta".into(),
                    persona: Some("agent-a".into()),
                    profile: None,
                    env: HashMap::new(),
                    weight: 1,
                    startup_phase: 1,
                },
                FleetSlot {
                    label: "alpha".into(),
                    persona: Some("agent-b".into()),
                    profile: None,
                    env: HashMap::new(),
                    weight: 2,
                    startup_phase: 0,
                },
                FleetSlot {
                    label: "beta".into(),
                    persona: None,
                    profile: Some("dev-shell".into()),
                    env: HashMap::new(),
                    weight: 2,
                    startup_phase: 0,
                },
            ],
            layout_template: Some("grid-2x2".into()),
            startup_strategy: FleetStartupStrategy::Serial,
            topology_profile: None,
            program_mix_targets: vec![
                FleetProgramTarget {
                    program: "codex-cli".into(),
                    weight: 2,
                },
                FleetProgramTarget {
                    program: "claude-code".into(),
                    weight: 1,
                },
                FleetProgramTarget {
                    program: "shell".into(),
                    weight: 2,
                },
            ],
        });

        let fleet = reg.resolve_fleet("ordering").expect("fleet resolves");
        assert_eq!(
            fleet.launch_plan.deterministic_order,
            vec!["alpha", "beta", "zeta"]
        );
        assert_eq!(
            fleet
                .panes
                .iter()
                .find(|p| p.label == "alpha")
                .unwrap()
                .launch_order,
            0
        );
        assert_eq!(
            fleet
                .panes
                .iter()
                .find(|p| p.label == "beta")
                .unwrap()
                .launch_order,
            1
        );
        assert_eq!(
            fleet
                .panes
                .iter()
                .find(|p| p.label == "zeta")
                .unwrap()
                .launch_order,
            2
        );
        assert_eq!(fleet.launch_plan.phases.len(), 2);
        assert_eq!(fleet.launch_plan.phases[0].phase, 0);
        assert_eq!(fleet.launch_plan.phases[0].slots, vec!["alpha", "beta"]);
        assert_eq!(fleet.launch_plan.phases[1].phase, 1);
        assert_eq!(fleet.launch_plan.phases[1].slots, vec!["zeta"]);
        // topology_profile is None; fall back to layout template.
        assert_eq!(
            fleet.launch_plan.topology_initializer.as_deref(),
            Some("grid-2x2")
        );
        assert_eq!(fleet.launch_plan.program_mix_deltas.len(), 3);
        assert_eq!(
            fleet.launch_plan.program_mix_deltas[0].program,
            "claude-code"
        );
        assert_eq!(fleet.launch_plan.program_mix_deltas[0].weight_delta, 0);
        assert_eq!(fleet.launch_plan.program_mix_deltas[1].program, "codex-cli");
        assert_eq!(fleet.launch_plan.program_mix_deltas[1].weight_delta, 0);
        assert_eq!(fleet.launch_plan.program_mix_deltas[2].program, "shell");
        assert_eq!(fleet.launch_plan.program_mix_deltas[2].weight_delta, 0);
        assert_eq!(
            fleet
                .launch_plan
                .slot_metadata
                .iter()
                .map(|slot| slot.label.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "beta", "zeta"]
        );
        assert!(fleet.launch_plan.invariant_violations().is_empty());
    }

    #[test]
    fn launch_plan_invariant_violations_emit_reason_codes() {
        let mut reg = ProfileRegistry::new();
        reg.register_defaults();
        reg.register_fleet_template(FleetTemplate {
            name: "broken-view".into(),
            description: None,
            slots: vec![
                FleetSlot {
                    label: "one".into(),
                    persona: None,
                    profile: Some("dev-shell".into()),
                    env: HashMap::new(),
                    weight: 2,
                    startup_phase: 0,
                },
                FleetSlot {
                    label: "two".into(),
                    persona: None,
                    profile: Some("monitor".into()),
                    env: HashMap::new(),
                    weight: 1,
                    startup_phase: 1,
                },
            ],
            layout_template: Some("split".into()),
            startup_strategy: FleetStartupStrategy::Phased,
            topology_profile: None,
            program_mix_targets: vec![FleetProgramTarget {
                program: "shell".into(),
                weight: 3,
            }],
        });

        let mut launch_plan = reg
            .resolve_fleet("broken-view")
            .expect("fleet resolves")
            .launch_plan;
        launch_plan.deterministic_order = vec!["two".into(), "one".into()];
        launch_plan.phases[0].slots = vec!["ghost".into()];
        launch_plan.total_weight = 1;
        launch_plan.program_mix[0].total_weight = 999;
        launch_plan.program_mix_deltas[0].actual_weight = 999;

        let violations = launch_plan.invariant_violations();
        let codes = violations
            .iter()
            .map(|violation| violation.reason_code.as_str())
            .collect::<Vec<_>>();

        assert!(codes.contains(&"fleet.launch_plan.deterministic_order_mismatch"));
        assert!(codes.contains(&"fleet.launch_plan.phase_slots_mismatch"));
        assert!(codes.contains(&"fleet.launch_plan.phase_slot_unknown"));
        assert!(codes.contains(&"fleet.launch_plan.total_weight_mismatch"));
        assert!(codes.contains(&"fleet.launch_plan.program_mix_mismatch"));
        assert!(codes.contains(&"fleet.launch_plan.program_mix_delta_mismatch"));
    }

    #[test]
    fn resolve_fleet_not_found() {
        let reg = ProfileRegistry::new();
        assert!(reg.resolve_fleet("nonexistent").is_none());
    }

    // -------------------------------------------------------------------------
    // Validation tests
    // -------------------------------------------------------------------------

    #[test]
    fn validate_ok() {
        let mut reg = ProfileRegistry::new();
        reg.register_defaults();
        assert!(reg.validate().is_empty());
    }

    #[test]
    fn validate_persona_missing_profile() {
        let mut reg = ProfileRegistry::new();
        reg.register_persona(Persona {
            name: "bad".into(),
            profile_name: "nonexistent".into(),
            env_overrides: HashMap::new(),
            agent_identity: None,
            description: None,
        });

        let errors = reg.validate();
        assert!(errors.iter().any(|e| matches!(
            e,
            ProfileValidationError::ProfileNotFound { persona, .. } if persona == "bad"
        )));
    }

    #[test]
    fn validate_fleet_empty_slots() {
        let mut reg = ProfileRegistry::new();
        reg.register_fleet_template(FleetTemplate {
            name: "empty".into(),
            description: None,
            slots: vec![],
            layout_template: None,
            startup_strategy: FleetStartupStrategy::Phased,
            topology_profile: None,
            program_mix_targets: vec![],
        });

        let errors = reg.validate();
        assert!(errors.iter().any(|e| matches!(
            e,
            ProfileValidationError::EmptyFleet { name } if name == "empty"
        )));
    }

    #[test]
    fn validate_fleet_slot_missing_persona() {
        let mut reg = ProfileRegistry::new();
        reg.register_fleet_template(FleetTemplate {
            name: "bad-fleet".into(),
            description: None,
            slots: vec![FleetSlot {
                label: "s1".into(),
                persona: Some("nonexistent".into()),
                profile: None,
                env: HashMap::new(),
                weight: 1,
                startup_phase: 0,
            }],
            layout_template: None,
            startup_strategy: FleetStartupStrategy::Phased,
            topology_profile: None,
            program_mix_targets: vec![],
        });

        let errors = reg.validate();
        assert!(errors.iter().any(|e| matches!(
            e,
            ProfileValidationError::SlotRefNotFound { fleet, .. } if fleet == "bad-fleet"
        )));
    }

    #[test]
    fn validate_duplicate_slot_labels_and_zero_weight() {
        let mut reg = ProfileRegistry::new();
        reg.register_defaults();
        reg.register_fleet_template(FleetTemplate {
            name: "bad-weights".into(),
            description: None,
            slots: vec![
                FleetSlot {
                    label: "dup".into(),
                    persona: None,
                    profile: Some("dev-shell".into()),
                    env: HashMap::new(),
                    weight: 0,
                    startup_phase: 0,
                },
                FleetSlot {
                    label: "dup".into(),
                    persona: None,
                    profile: Some("monitor".into()),
                    env: HashMap::new(),
                    weight: 1,
                    startup_phase: 1,
                },
            ],
            layout_template: None,
            startup_strategy: FleetStartupStrategy::Phased,
            topology_profile: None,
            program_mix_targets: vec![],
        });

        let errors = reg.validate();
        assert!(errors.iter().any(|e| matches!(
            e,
            ProfileValidationError::DuplicateSlotLabel { fleet, label }
                if fleet == "bad-weights" && label == "dup"
        )));
        assert!(errors.iter().any(|e| matches!(
            e,
            ProfileValidationError::InvalidSlotWeight { fleet, slot }
                if fleet == "bad-weights" && slot == "dup"
        )));
    }

    #[test]
    fn validate_program_mix_target_errors() {
        let mut reg = ProfileRegistry::new();
        reg.register_defaults();
        reg.register_fleet_template(FleetTemplate {
            name: "bad-program-mix".into(),
            description: None,
            slots: vec![FleetSlot {
                label: "primary".into(),
                persona: None,
                profile: Some("dev-shell".into()),
                env: HashMap::new(),
                weight: 1,
                startup_phase: 0,
            }],
            layout_template: Some("side-by-side".into()),
            startup_strategy: FleetStartupStrategy::Phased,
            topology_profile: None,
            program_mix_targets: vec![
                FleetProgramTarget {
                    program: " ".into(),
                    weight: 1,
                },
                FleetProgramTarget {
                    program: "codex-cli".into(),
                    weight: 0,
                },
                FleetProgramTarget {
                    program: "codex-cli".into(),
                    weight: 2,
                },
            ],
        });

        let errors = reg.validate();
        assert!(errors.iter().any(|e| matches!(
            e,
            ProfileValidationError::EmptyProgramMixTarget { fleet } if fleet == "bad-program-mix"
        )));
        assert!(errors.iter().any(|e| matches!(
            e,
            ProfileValidationError::InvalidProgramMixWeight { fleet, program }
                if fleet == "bad-program-mix" && program == "codex-cli"
        )));
        assert!(errors.iter().any(|e| matches!(
            e,
            ProfileValidationError::DuplicateProgramMixTarget { fleet, program }
                if fleet == "bad-program-mix" && program == "codex-cli"
        )));
    }

    #[test]
    fn validate_empty_bootstrap_command() {
        let mut reg = ProfileRegistry::new();
        reg.register_profile(SessionProfile {
            name: "bad-boot".into(),
            description: None,
            role: ProfileRole::Custom,
            spawn_command: None,
            environment: HashMap::new(),
            working_directory: None,
            resource_hints: ResourceHints::default(),
            policy: ProfilePolicy::default(),
            layout_template: None,
            bootstrap_commands: vec!["ok".into(), "  ".into()],
            tags: vec![],
            updated_at: 0,
        });

        let errors = reg.validate();
        assert!(errors.iter().any(|e| matches!(
            e,
            ProfileValidationError::EmptyBootstrapCommand { profile, index } if profile == "bad-boot" && *index == 1
        )));
    }

    // -------------------------------------------------------------------------
    // ProfileValidationError display tests
    // -------------------------------------------------------------------------

    #[test]
    fn validation_error_display() {
        let err = ProfileValidationError::EmptyName;
        assert!(err.to_string().contains("empty"));

        let err = ProfileValidationError::ProfileNotFound {
            persona: "p".into(),
            profile: "q".into(),
        };
        assert!(err.to_string().contains("p"));
        assert!(err.to_string().contains("q"));

        let err = ProfileValidationError::EmptyFleet { name: "f".into() };
        assert!(err.to_string().contains("f"));
    }

    // -------------------------------------------------------------------------
    // SpawnCommand tests
    // -------------------------------------------------------------------------

    #[test]
    fn spawn_command_serde() {
        let cmd = SpawnCommand {
            command: "cargo".into(),
            args: vec!["test".into(), "--release".into()],
            use_shell: false,
        };

        let json = serde_json::to_string(&cmd).unwrap();
        let deserialized: SpawnCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, deserialized);
    }

    #[test]
    fn spawn_command_use_shell_default() {
        let json = r#"{"command":"echo"}"#;
        let cmd: SpawnCommand = serde_json::from_str(json).unwrap();
        assert!(cmd.use_shell);
        assert!(cmd.args.is_empty());
    }

    // -------------------------------------------------------------------------
    // AgentIdentitySpec tests
    // -------------------------------------------------------------------------

    #[test]
    fn agent_identity_spec_serde() {
        let spec = AgentIdentitySpec {
            program: "claude-code".into(),
            model: Some("opus-4.1".into()),
            task: Some("Bug fixing".into()),
        };

        let json = serde_json::to_string(&spec).unwrap();
        let deserialized: AgentIdentitySpec = serde_json::from_str(&json).unwrap();
        assert_eq!(spec, deserialized);
    }

    #[test]
    fn agent_identity_spec_minimal() {
        let json = r#"{"program":"codex-cli"}"#;
        let spec: AgentIdentitySpec = serde_json::from_str(json).unwrap();
        assert_eq!(spec.program, "codex-cli");
        assert!(spec.model.is_none());
        assert!(spec.task.is_none());
    }
}
