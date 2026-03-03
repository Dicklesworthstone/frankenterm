// =============================================================================
// Session profile/template/persona engine (ft-3681t.2.4)
//
// Declarative spawn profiles for fleet setup: role defaults, command bootstraps,
// environment setup, resource hints, and policy posture. Makes fleet provisioning
// reproducible and codified rather than ad-hoc.
// =============================================================================

use std::collections::HashMap;
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
}

/// A single slot in a fleet template.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
                });
            }
        }

        Some(ResolvedFleet {
            name: fleet.name.clone(),
            layout_template: fleet.layout_template.clone(),
            panes,
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
    pub panes: Vec<ResolvedFleetPane>,
}

/// A single resolved pane in a fleet.
#[derive(Debug, Clone)]
pub struct ResolvedFleetPane {
    pub label: String,
    pub resolved: ResolvedProfile,
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
    SlotRefNotFound { fleet: String, slot: String, ref_name: String },
    /// Fleet has no slots.
    EmptyFleet { name: String },
    /// Bootstrap command is empty.
    EmptyBootstrapCommand { profile: String, index: usize },
}

impl std::fmt::Display for ProfileValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyName => write!(f, "profile name cannot be empty"),
            Self::ProfileNotFound { persona, profile } => {
                write!(f, "persona '{persona}' references unknown profile '{profile}'")
            }
            Self::SlotRefNotFound { fleet, slot, ref_name } => {
                write!(
                    f,
                    "fleet '{fleet}' slot '{slot}' references unknown '{ref_name}'"
                )
            }
            Self::EmptyFleet { name } => {
                write!(f, "fleet template '{name}' has no slots")
            }
            Self::EmptyBootstrapCommand { profile, index } => {
                write!(f, "profile '{profile}' has empty bootstrap command at index {index}")
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
            for slot in &fleet.slots {
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
                },
                FleetSlot {
                    label: "worker-1".into(),
                    persona: Some("builder".into()),
                    profile: None,
                    env: HashMap::new(),
                },
            ],
            layout_template: Some("side-by-side".into()),
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
            agent_identity: None,
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
                },
            ],
            layout_template: Some("side-by-side".into()),
        });

        let fleet = reg.resolve_fleet("my-fleet").unwrap();
        assert_eq!(fleet.name, "my-fleet");
        assert_eq!(fleet.panes.len(), 2);
        assert_eq!(fleet.panes[0].label, "primary");
        assert_eq!(fleet.panes[0].resolved.profile.role, ProfileRole::DevShell);
        assert_eq!(fleet.panes[1].label, "worker");
        assert_eq!(
            fleet.panes[1].resolved.environment.get("SLOT_ID").map(|s| s.as_str()),
            Some("w1")
        );
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
            }],
            layout_template: None,
        });

        let errors = reg.validate();
        assert!(errors.iter().any(|e| matches!(
            e,
            ProfileValidationError::SlotRefNotFound { fleet, .. } if fleet == "bad-fleet"
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
