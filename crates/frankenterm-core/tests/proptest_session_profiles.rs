//! Property-based tests for session_profiles (ft-3681t.2.4).
//!
//! Coverage: serde roundtrips for all profile types, ProfileRegistry
//! register/resolve properties, FleetTemplate resolve and launch plan
//! invariants, validation error detection, and deterministic ordering.

use std::collections::HashMap;

use proptest::prelude::*;

use frankenterm_core::session_profiles::{
    AgentIdentitySpec, FleetLaunchPlan, FleetProgramMix, FleetProgramMixDelta, FleetProgramTarget,
    FleetSlot, FleetStartupStrategy, FleetTemplate, Persona, ProfilePolicy, ProfileRegistry,
    ProfileRole, ResourceHints, SessionProfile, SpawnCommand,
};

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn arb_profile_role() -> impl Strategy<Value = ProfileRole> {
    prop_oneof![
        Just(ProfileRole::DevShell),
        Just(ProfileRole::AgentWorker),
        Just(ProfileRole::Monitor),
        Just(ProfileRole::BuildRunner),
        Just(ProfileRole::TestRunner),
        Just(ProfileRole::Service),
        Just(ProfileRole::Custom),
    ]
}

fn arb_startup_strategy() -> impl Strategy<Value = FleetStartupStrategy> {
    prop_oneof![
        Just(FleetStartupStrategy::Phased),
        Just(FleetStartupStrategy::Serial),
    ]
}

fn arb_resource_hints() -> impl Strategy<Value = ResourceHints> {
    (
        1u16..100,
        10u16..200,
        prop::option::of(10u16..100),
        prop::option::of(40u16..200),
        100u32..100_000,
        1u32..10,
    )
        .prop_map(
            |(min_rows, min_cols, pref_rows, pref_cols, scrollback, weight)| ResourceHints {
                min_rows,
                min_cols,
                preferred_rows: pref_rows,
                preferred_cols: pref_cols,
                max_scrollback: scrollback,
                priority_weight: weight,
            },
        )
}

fn arb_profile_policy() -> impl Strategy<Value = ProfilePolicy> {
    (
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        0u64..7200,
    )
        .prop_map(
            |(input, capture, interrupt, auto_close, audit, idle)| ProfilePolicy {
                allow_input: input,
                allow_capture: capture,
                allow_interrupt: interrupt,
                allow_auto_close: auto_close,
                audit_commands: audit,
                idle_timeout_secs: idle,
            },
        )
}

fn arb_spawn_command() -> impl Strategy<Value = SpawnCommand> {
    ("[a-z]{3,10}", prop::collection::vec("[a-z]{1,8}", 0..3), any::<bool>()).prop_map(
        |(command, args, use_shell)| SpawnCommand {
            command,
            args,
            use_shell,
        },
    )
}

fn arb_agent_identity() -> impl Strategy<Value = AgentIdentitySpec> {
    (
        "[a-z-]{3,12}",
        prop::option::of("[a-z0-9.]{3,10}"),
        prop::option::of("[a-z ]{3,15}"),
    )
        .prop_map(|(program, model, task)| AgentIdentitySpec {
            program,
            model,
            task,
        })
}

fn arb_session_profile(name: impl Strategy<Value = String>) -> impl Strategy<Value = SessionProfile>
{
    (
        name,
        prop::option::of("[a-z ]{5,20}"),
        arb_profile_role(),
        prop::option::of(arb_spawn_command()),
        arb_resource_hints(),
        arb_profile_policy(),
        prop::option::of("[a-z-]{3,10}"),
        prop::collection::vec("[a-z]{3,10}", 0..3),
        prop::collection::vec("[a-z]{3,8}", 0..3),
        0u64..u64::MAX / 2,
    )
        .prop_map(
            |(
                name,
                description,
                role,
                spawn_command,
                resource_hints,
                policy,
                layout_template,
                bootstrap_commands,
                tags,
                updated_at,
            )| SessionProfile {
                name,
                description,
                role,
                spawn_command,
                environment: HashMap::new(),
                working_directory: None,
                resource_hints,
                policy,
                layout_template,
                bootstrap_commands,
                tags,
                updated_at,
            },
        )
}

// ---------------------------------------------------------------------------
// Serde roundtrips
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn profile_role_serde_roundtrip(role in arb_profile_role()) {
        let json = serde_json::to_string(&role).unwrap();
        let decoded: ProfileRole = serde_json::from_str(&json).unwrap();
        assert_eq!(role, decoded);
    }

    #[test]
    fn startup_strategy_serde_roundtrip(strat in arb_startup_strategy()) {
        let json = serde_json::to_string(&strat).unwrap();
        let decoded: FleetStartupStrategy = serde_json::from_str(&json).unwrap();
        assert_eq!(strat, decoded);
    }

    #[test]
    fn resource_hints_serde_roundtrip(hints in arb_resource_hints()) {
        let json = serde_json::to_string(&hints).unwrap();
        let decoded: ResourceHints = serde_json::from_str(&json).unwrap();
        assert_eq!(hints, decoded);
    }

    #[test]
    fn profile_policy_serde_roundtrip(policy in arb_profile_policy()) {
        let json = serde_json::to_string(&policy).unwrap();
        let decoded: ProfilePolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(policy, decoded);
    }

    #[test]
    fn spawn_command_serde_roundtrip(cmd in arb_spawn_command()) {
        let json = serde_json::to_string(&cmd).unwrap();
        let decoded: SpawnCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, decoded);
    }

    #[test]
    fn agent_identity_spec_serde_roundtrip(spec in arb_agent_identity()) {
        let json = serde_json::to_string(&spec).unwrap();
        let decoded: AgentIdentitySpec = serde_json::from_str(&json).unwrap();
        assert_eq!(spec, decoded);
    }

    #[test]
    fn session_profile_serde_roundtrip(profile in arb_session_profile("[a-z-]{3,10}".prop_map(|s| s))) {
        let json = serde_json::to_string(&profile).unwrap();
        let decoded: SessionProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(profile, decoded);
    }

    #[test]
    fn fleet_startup_strategy_default_is_phased(_dummy in 0u8..1) {
        let strat: FleetStartupStrategy = Default::default();
        assert_eq!(strat, FleetStartupStrategy::Phased);
    }

    #[test]
    fn fleet_program_target_serde_roundtrip(
        program in "[a-z-]{3,10}",
        weight in 1u32..100,
    ) {
        let target = FleetProgramTarget { program, weight };
        let json = serde_json::to_string(&target).unwrap();
        let decoded: FleetProgramTarget = serde_json::from_str(&json).unwrap();
        assert_eq!(target, decoded);
    }
}

// ---------------------------------------------------------------------------
// ProfileRegistry properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    /// Registering n distinct profiles yields profile_count() == n.
    #[test]
    fn registry_profile_count(
        names in prop::collection::hash_set("[a-z]{3,10}", 1..8),
    ) {
        let mut reg = ProfileRegistry::new();
        for name in &names {
            reg.register_profile(SessionProfile {
                name: name.clone(),
                description: None,
                role: ProfileRole::DevShell,
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
        }
        assert_eq!(reg.profile_count(), names.len());
    }

    /// Overwriting a profile doesn't increase count.
    #[test]
    fn registry_profile_overwrite_stable(name in "[a-z]{3,10}") {
        let mut reg = ProfileRegistry::new();
        for i in 0..5 {
            reg.register_profile(SessionProfile {
                name: name.clone(),
                description: Some(format!("v{i}")),
                role: ProfileRole::Monitor,
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
        }
        assert_eq!(reg.profile_count(), 1);
        assert_eq!(
            reg.get_profile(&name).unwrap().description.as_deref(),
            Some("v4")
        );
    }

    /// profile_names() returns sorted output.
    #[test]
    fn registry_profile_names_sorted(
        names in prop::collection::hash_set("[a-z]{3,10}", 1..8),
    ) {
        let mut reg = ProfileRegistry::new();
        for name in &names {
            reg.register_profile(SessionProfile {
                name: name.clone(),
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
        }
        let result = reg.profile_names();
        let mut sorted = result.clone();
        sorted.sort_unstable();
        assert_eq!(result, sorted);
    }

    /// persona_names() returns sorted output.
    #[test]
    fn registry_persona_names_sorted(
        names in prop::collection::hash_set("[a-z]{3,10}", 1..8),
    ) {
        let mut reg = ProfileRegistry::new();
        reg.register_profile(SessionProfile {
            name: "base".into(),
            description: None,
            role: ProfileRole::AgentWorker,
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
        for name in &names {
            reg.register_persona(Persona {
                name: name.clone(),
                profile_name: "base".into(),
                env_overrides: HashMap::new(),
                agent_identity: None,
                description: None,
            });
        }
        let result = reg.persona_names();
        let mut sorted = result.clone();
        sorted.sort_unstable();
        assert_eq!(result, sorted);
    }
}

// ---------------------------------------------------------------------------
// Resolve persona properties
// ---------------------------------------------------------------------------

#[test]
fn resolve_persona_merges_env() {
    let mut reg = ProfileRegistry::new();

    let mut base_env = HashMap::new();
    base_env.insert("KEY_A".into(), "from_profile".into());
    base_env.insert("KEY_B".into(), "from_profile".into());

    reg.register_profile(SessionProfile {
        name: "worker".into(),
        description: None,
        role: ProfileRole::AgentWorker,
        spawn_command: None,
        environment: base_env,
        working_directory: None,
        resource_hints: ResourceHints::default(),
        policy: ProfilePolicy::default(),
        layout_template: None,
        bootstrap_commands: vec![],
        tags: vec![],
        updated_at: 0,
    });

    let mut overrides = HashMap::new();
    overrides.insert("KEY_B".into(), "from_persona".into());
    overrides.insert("KEY_C".into(), "from_persona".into());

    reg.register_persona(Persona {
        name: "my-agent".into(),
        profile_name: "worker".into(),
        env_overrides: overrides,
        agent_identity: Some(AgentIdentitySpec {
            program: "claude-code".into(),
            model: Some("opus-4.1".into()),
            task: None,
        }),
        description: None,
    });

    let resolved = reg.resolve_persona("my-agent").unwrap();
    assert_eq!(resolved.environment["KEY_A"], "from_profile");
    assert_eq!(resolved.environment["KEY_B"], "from_persona"); // overridden
    assert_eq!(resolved.environment["KEY_C"], "from_persona"); // added
    assert!(resolved.agent_identity.is_some());
    assert_eq!(resolved.agent_identity.unwrap().program, "claude-code");
}

#[test]
fn resolve_persona_missing_profile_returns_none() {
    let mut reg = ProfileRegistry::new();
    reg.register_persona(Persona {
        name: "orphan".into(),
        profile_name: "nonexistent".into(),
        env_overrides: HashMap::new(),
        agent_identity: None,
        description: None,
    });
    assert!(reg.resolve_persona("orphan").is_none());
}

#[test]
fn resolve_persona_missing_persona_returns_none() {
    let reg = ProfileRegistry::new();
    assert!(reg.resolve_persona("nonexistent").is_none());
}

// ---------------------------------------------------------------------------
// Fleet resolve and launch plan properties
// ---------------------------------------------------------------------------

#[test]
fn resolve_fleet_deterministic_ordering() {
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
        name: "test-fleet".into(),
        description: None,
        slots: vec![
            FleetSlot {
                label: "worker-b".into(),
                persona: Some("agent-b".into()),
                profile: None,
                env: HashMap::new(),
                weight: 1,
                startup_phase: 1,
            },
            FleetSlot {
                label: "worker-a".into(),
                persona: Some("agent-a".into()),
                profile: None,
                env: HashMap::new(),
                weight: 2,
                startup_phase: 0,
            },
            FleetSlot {
                label: "monitor".into(),
                persona: None,
                profile: Some("monitor".into()),
                env: HashMap::new(),
                weight: 1,
                startup_phase: 0,
            },
        ],
        layout_template: Some("primary-sidebar".into()),
        startup_strategy: FleetStartupStrategy::Phased,
        topology_profile: None,
        program_mix_targets: vec![],
    });

    let fleet = reg.resolve_fleet("test-fleet").unwrap();

    // Launch plan should be deterministic
    let plan = &fleet.launch_plan;
    assert_eq!(plan.fleet, "test-fleet");

    // All invariants should pass
    let violations = plan.invariant_violations();
    assert!(
        violations.is_empty(),
        "invariant violations: {violations:?}"
    );

    // Phase 0 comes before phase 1
    assert_eq!(plan.phases.len(), 2);
    assert_eq!(plan.phases[0].phase, 0);
    assert_eq!(plan.phases[1].phase, 1);

    // Within phase 0: worker-a (weight=2) before monitor (weight=1)
    assert_eq!(plan.phases[0].slots[0], "worker-a");
    assert_eq!(plan.phases[0].slots[1], "monitor");

    // Total weight = 2 + 1 + 1 = 4
    assert_eq!(plan.total_weight, 4);
}

#[test]
fn resolve_fleet_invariant_violations_always_empty() {
    let mut reg = ProfileRegistry::new();
    reg.register_defaults();

    reg.register_fleet_template(FleetTemplate {
        name: "simple".into(),
        description: None,
        slots: vec![
            FleetSlot {
                label: "dev".into(),
                persona: None,
                profile: Some("dev-shell".into()),
                env: HashMap::new(),
                weight: 1,
                startup_phase: 0,
            },
            FleetSlot {
                label: "build".into(),
                persona: None,
                profile: Some("build-runner".into()),
                env: HashMap::new(),
                weight: 3,
                startup_phase: 0,
            },
        ],
        layout_template: None,
        startup_strategy: FleetStartupStrategy::Serial,
        topology_profile: None,
        program_mix_targets: vec![],
    });

    let fleet = reg.resolve_fleet("simple").unwrap();
    let violations = fleet.launch_plan.invariant_violations();
    assert!(violations.is_empty(), "violations: {violations:?}");
}

#[test]
fn resolve_fleet_missing_template_returns_none() {
    let reg = ProfileRegistry::new();
    assert!(reg.resolve_fleet("nonexistent").is_none());
}

#[test]
fn resolve_fleet_slot_env_override() {
    let mut reg = ProfileRegistry::new();
    reg.register_defaults();

    let mut slot_env = HashMap::new();
    slot_env.insert("SLOT_VAR".into(), "slot_value".into());

    reg.register_fleet_template(FleetTemplate {
        name: "env-test".into(),
        description: None,
        slots: vec![FleetSlot {
            label: "pane-1".into(),
            persona: None,
            profile: Some("dev-shell".into()),
            env: slot_env,
            weight: 1,
            startup_phase: 0,
        }],
        layout_template: None,
        startup_strategy: FleetStartupStrategy::Phased,
        topology_profile: None,
        program_mix_targets: vec![],
    });

    let fleet = reg.resolve_fleet("env-test").unwrap();
    assert_eq!(fleet.panes.len(), 1);
    assert_eq!(
        fleet.panes[0].resolved.environment.get("SLOT_VAR").map(|s| s.as_str()),
        Some("slot_value")
    );
}

// ---------------------------------------------------------------------------
// Program mix delta properties
// ---------------------------------------------------------------------------

#[test]
fn program_mix_delta_zero_when_no_targets() {
    let mut reg = ProfileRegistry::new();
    reg.register_defaults();

    reg.register_fleet_template(FleetTemplate {
        name: "no-targets".into(),
        description: None,
        slots: vec![FleetSlot {
            label: "pane".into(),
            persona: None,
            profile: Some("dev-shell".into()),
            env: HashMap::new(),
            weight: 1,
            startup_phase: 0,
        }],
        layout_template: None,
        startup_strategy: FleetStartupStrategy::Phased,
        topology_profile: None,
        program_mix_targets: vec![], // no targets
    });

    let fleet = reg.resolve_fleet("no-targets").unwrap();
    let plan = &fleet.launch_plan;

    // With no targets, all deltas should have target_weight=0
    for delta in &plan.program_mix_deltas {
        assert_eq!(delta.target_weight, 0);
        assert!(delta.weight_delta >= 0);
    }
}

#[test]
fn program_mix_delta_tracks_target_difference() {
    let mut reg = ProfileRegistry::new();
    reg.register_defaults();

    reg.register_persona(Persona {
        name: "cc-agent".into(),
        profile_name: "agent-worker".into(),
        env_overrides: HashMap::new(),
        agent_identity: Some(AgentIdentitySpec {
            program: "claude-code".into(),
            model: None,
            task: None,
        }),
        description: None,
    });

    reg.register_fleet_template(FleetTemplate {
        name: "mix-test".into(),
        description: None,
        slots: vec![FleetSlot {
            label: "cc".into(),
            persona: Some("cc-agent".into()),
            profile: None,
            env: HashMap::new(),
            weight: 2,
            startup_phase: 0,
        }],
        layout_template: None,
        startup_strategy: FleetStartupStrategy::Phased,
        topology_profile: None,
        program_mix_targets: vec![FleetProgramTarget {
            program: "claude-code".into(),
            weight: 5,
        }],
    });

    let fleet = reg.resolve_fleet("mix-test").unwrap();
    let plan = &fleet.launch_plan;

    let cc_delta = plan
        .program_mix_deltas
        .iter()
        .find(|d| d.program == "claude-code")
        .unwrap();
    assert_eq!(cc_delta.target_weight, 5);
    assert_eq!(cc_delta.actual_weight, 2);
    assert_eq!(cc_delta.weight_delta, -3); // 2 - 5 = -3
}

// ---------------------------------------------------------------------------
// Validation properties
// ---------------------------------------------------------------------------

#[test]
fn validation_defaults_clean() {
    let mut reg = ProfileRegistry::new();
    reg.register_defaults();
    let errors = reg.validate();
    assert!(errors.is_empty(), "default profiles should validate clean: {errors:?}");
}

#[test]
fn validation_catches_empty_profile_name() {
    let mut reg = ProfileRegistry::new();
    reg.register_profile(SessionProfile {
        name: "".into(),
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
    let errors = reg.validate();
    assert!(errors.iter().any(|e| matches!(e, frankenterm_core::session_profiles::ProfileValidationError::EmptyName)));
}

#[test]
fn validation_catches_orphan_persona() {
    let mut reg = ProfileRegistry::new();
    reg.register_persona(Persona {
        name: "orphan".into(),
        profile_name: "nonexistent".into(),
        env_overrides: HashMap::new(),
        agent_identity: None,
        description: None,
    });
    let errors = reg.validate();
    assert!(errors.iter().any(|e| matches!(e, frankenterm_core::session_profiles::ProfileValidationError::ProfileNotFound { .. })));
}

#[test]
fn validation_catches_empty_fleet() {
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
    assert!(errors.iter().any(|e| matches!(e, frankenterm_core::session_profiles::ProfileValidationError::EmptyFleet { .. })));
}

#[test]
fn validation_catches_duplicate_slot_labels() {
    let mut reg = ProfileRegistry::new();
    reg.register_defaults();
    reg.register_fleet_template(FleetTemplate {
        name: "dupe".into(),
        description: None,
        slots: vec![
            FleetSlot {
                label: "same".into(),
                persona: None,
                profile: Some("dev-shell".into()),
                env: HashMap::new(),
                weight: 1,
                startup_phase: 0,
            },
            FleetSlot {
                label: "same".into(),
                persona: None,
                profile: Some("dev-shell".into()),
                env: HashMap::new(),
                weight: 1,
                startup_phase: 0,
            },
        ],
        layout_template: None,
        startup_strategy: FleetStartupStrategy::Phased,
        topology_profile: None,
        program_mix_targets: vec![],
    });
    let errors = reg.validate();
    assert!(errors.iter().any(|e| matches!(e, frankenterm_core::session_profiles::ProfileValidationError::DuplicateSlotLabel { .. })));
}

#[test]
fn validation_catches_empty_bootstrap_command() {
    let mut reg = ProfileRegistry::new();
    reg.register_profile(SessionProfile {
        name: "bad-bootstrap".into(),
        description: None,
        role: ProfileRole::Custom,
        spawn_command: None,
        environment: HashMap::new(),
        working_directory: None,
        resource_hints: ResourceHints::default(),
        policy: ProfilePolicy::default(),
        layout_template: None,
        bootstrap_commands: vec!["echo ok".into(), "  ".into()],
        tags: vec![],
        updated_at: 0,
    });
    let errors = reg.validate();
    assert!(errors.iter().any(|e| matches!(e, frankenterm_core::session_profiles::ProfileValidationError::EmptyBootstrapCommand { .. })));
}

// ---------------------------------------------------------------------------
// FleetLaunchPlan serde roundtrip
// ---------------------------------------------------------------------------

#[test]
fn fleet_launch_plan_serde_roundtrip() {
    let plan = FleetLaunchPlan {
        fleet: "test".into(),
        startup_strategy: FleetStartupStrategy::Phased,
        topology_initializer: Some("grid-2x2".into()),
        deterministic_order: vec!["a".into(), "b".into()],
        phases: vec![frankenterm_core::session_profiles::FleetLaunchPhase {
            phase: 0,
            slots: vec!["a".into(), "b".into()],
        }],
        slot_metadata: vec![
            frankenterm_core::session_profiles::FleetLaunchSlotMetadata {
                label: "a".into(),
                startup_phase: 0,
                launch_order: 0,
                weight: 2,
                program: "claude-code".into(),
                persona: "agent-a".into(),
                profile: "agent-worker".into(),
            },
            frankenterm_core::session_profiles::FleetLaunchSlotMetadata {
                label: "b".into(),
                startup_phase: 0,
                launch_order: 1,
                weight: 1,
                program: "shell".into(),
                persona: "dev-shell".into(),
                profile: "dev-shell".into(),
            },
        ],
        total_weight: 3,
        program_mix: vec![
            FleetProgramMix {
                program: "claude-code".into(),
                slot_count: 1,
                total_weight: 2,
                slots: vec!["a".into()],
            },
            FleetProgramMix {
                program: "shell".into(),
                slot_count: 1,
                total_weight: 1,
                slots: vec!["b".into()],
            },
        ],
        program_mix_deltas: vec![FleetProgramMixDelta {
            program: "claude-code".into(),
            target_weight: 5,
            actual_weight: 2,
            actual_slots: 1,
            weight_delta: -3,
        }],
    };

    let json = serde_json::to_string(&plan).unwrap();
    let decoded: FleetLaunchPlan = serde_json::from_str(&json).unwrap();
    assert_eq!(plan, decoded);
}

// ---------------------------------------------------------------------------
// Default profiles
// ---------------------------------------------------------------------------

#[test]
fn default_profiles_registered() {
    let mut reg = ProfileRegistry::new();
    reg.register_defaults();

    assert!(reg.get_profile("dev-shell").is_some());
    assert!(reg.get_profile("agent-worker").is_some());
    assert!(reg.get_profile("monitor").is_some());
    assert!(reg.get_profile("build-runner").is_some());
    assert!(reg.profile_count() >= 4);
}

#[test]
fn default_resource_hints_have_sane_values() {
    let hints = ResourceHints::default();
    assert!(hints.min_rows > 0);
    assert!(hints.min_cols > 0);
    assert!(hints.max_scrollback > 0);
    assert!(hints.priority_weight > 0);
}

#[test]
fn default_policy_allows_all_except_audit() {
    let policy = ProfilePolicy::default();
    assert!(policy.allow_input);
    assert!(policy.allow_capture);
    assert!(policy.allow_interrupt);
    assert!(policy.allow_auto_close);
    assert!(!policy.audit_commands);
    assert_eq!(policy.idle_timeout_secs, 0);
}
