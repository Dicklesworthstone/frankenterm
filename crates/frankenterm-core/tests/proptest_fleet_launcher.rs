//! Property-based tests for the fleet launcher module.

use proptest::prelude::*;
use std::collections::HashMap;

use frankenterm_core::command_transport::CommandRouter;
use frankenterm_core::durable_state::DurableStateManager;
use frankenterm_core::fleet_launcher::{
    AgentMixEntry, FleetLaunchError, FleetLaunchStatus, FleetLauncher, FleetSpec, SlotStatus,
    StartupStrategy,
};
use frankenterm_core::session_profiles::{ProfileRegistry, ProfileRole};
use frankenterm_core::session_topology::{
    LifecycleRegistry, LifecycleState, MuxPaneLifecycleState,
};

fn arb_startup_strategy() -> impl Strategy<Value = StartupStrategy> {
    prop_oneof![
        Just(StartupStrategy::Parallel),
        Just(StartupStrategy::Sequential),
        Just(StartupStrategy::Phased),
    ]
}

fn arb_profile_role() -> impl Strategy<Value = ProfileRole> {
    prop_oneof![
        Just(ProfileRole::Service),
        Just(ProfileRole::Monitor),
        Just(ProfileRole::BuildRunner),
        Just(ProfileRole::AgentWorker),
        Just(ProfileRole::TestRunner),
        Just(ProfileRole::DevShell),
        Just(ProfileRole::Custom),
    ]
}

fn arb_mix_entry() -> impl Strategy<Value = AgentMixEntry> {
    (
        prop_oneof![
            Just("claude-code".to_string()),
            Just("codex-cli".to_string()),
            Just("gemini-cli".to_string()),
        ],
        proptest::option::of(prop_oneof![
            Just("opus-4.1".to_string()),
            Just("gpt5-codex".to_string()),
        ]),
        1u32..10u32,
        proptest::option::of(arb_profile_role()),
    )
        .prop_map(|(program, model, weight, role)| AgentMixEntry {
            program,
            model,
            weight,
            profile: None, // use default "agent-worker"
            task_template: None,
            environment: HashMap::new(),
            role,
        })
}

fn arb_fleet_spec(min_entries: usize, max_entries: usize) -> impl Strategy<Value = FleetSpec> {
    (
        prop_oneof![
            Just("fleet-alpha".to_string()),
            Just("fleet-beta".to_string()),
            Just("fleet-gamma".to_string()),
        ],
        proptest::collection::vec(arb_mix_entry(), min_entries..=max_entries),
        arb_startup_strategy(),
        0u32..20u32,
        1u64..100u64,
    )
        .prop_map(|(name, mix, strategy, total_panes, generation)| FleetSpec {
            name,
            description: None,
            workspace_id: "test-ws".to_string(),
            domain: "local".to_string(),
            mix,
            total_panes,
            fleet_template: None,
            working_directory: None,
            startup_strategy: strategy,
            generation,
            tags: vec![],
        })
}

fn test_registry() -> ProfileRegistry {
    let mut reg = ProfileRegistry::new();
    reg.register_defaults();
    reg
}

proptest! {
    #[test]
    fn startup_strategy_serde_roundtrip(strategy in arb_startup_strategy()) {
        let json = serde_json::to_string(&strategy).unwrap();
        let back: StartupStrategy = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(strategy, back);
    }

    #[test]
    fn slot_status_serde_roundtrip(status in prop_oneof![
        Just(SlotStatus::Registered),
        Just(SlotStatus::Failed),
        Just(SlotStatus::Skipped),
    ]) {
        let json = serde_json::to_string(&status).unwrap();
        let back: SlotStatus = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(status, back);
    }

    #[test]
    fn fleet_launch_status_serde_roundtrip(status in prop_oneof![
        Just(FleetLaunchStatus::Complete),
        Just(FleetLaunchStatus::Partial),
        Just(FleetLaunchStatus::Failed),
    ]) {
        let json = serde_json::to_string(&status).unwrap();
        let back: FleetLaunchStatus = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(status, back);
    }

    #[test]
    fn fleet_spec_serde_roundtrip(spec in arb_fleet_spec(1, 5)) {
        let json = serde_json::to_string(&spec).unwrap();
        let back: FleetSpec = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(spec, back);
    }

    #[test]
    fn plan_slot_count_equals_total_panes_when_specified(
        spec in arb_fleet_spec(1, 5).prop_filter(
            "total_panes must be > 0",
            |s| s.total_panes > 0
        )
    ) {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let plan = launcher.plan(&spec).unwrap();
        prop_assert_eq!(plan.slots.len() as u32, spec.total_panes);
    }

    #[test]
    fn plan_slot_count_equals_total_weight_when_no_override(
        mix in proptest::collection::vec(arb_mix_entry(), 1..=5),
        strategy in arb_startup_strategy(),
    ) {
        let total_weight: u32 = mix.iter().map(|e| e.weight).sum();
        if total_weight == 0 {
            return Ok(());
        }
        let spec = FleetSpec {
            name: "weight-test".to_string(),
            description: None,
            workspace_id: "ws".to_string(),
            domain: "local".to_string(),
            mix,
            total_panes: 0,
            fleet_template: None,
            working_directory: None,
            startup_strategy: strategy,
            generation: 1,
            tags: vec![],
        };
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let plan = launcher.plan(&spec).unwrap();
        prop_assert_eq!(plan.slots.len() as u32, total_weight);
    }

    #[test]
    fn plan_lifecycle_identities_unique(spec in arb_fleet_spec(1, 5)) {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let plan = launcher.plan(&spec).unwrap();
        let keys: Vec<String> = plan.slots.iter().map(|s| s.lifecycle_identity.stable_key()).collect();
        let unique: std::collections::HashSet<&str> = keys.iter().map(|s| s.as_str()).collect();
        prop_assert_eq!(keys.len(), unique.len(), "lifecycle identities must be unique");
    }

    #[test]
    fn plan_labels_unique(spec in arb_fleet_spec(1, 5)) {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let plan = launcher.plan(&spec).unwrap();
        let labels: Vec<&str> = plan.slots.iter().map(|s| s.label.as_str()).collect();
        let unique: std::collections::HashSet<&str> = labels.iter().copied().collect();
        prop_assert_eq!(labels.len(), unique.len(), "slot labels must be unique");
    }

    #[test]
    fn plan_slot_indices_sequential(spec in arb_fleet_spec(1, 5)) {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let plan = launcher.plan(&spec).unwrap();
        for (i, slot) in plan.slots.iter().enumerate() {
            prop_assert_eq!(slot.index, i as u32, "slot indices must be sequential");
        }
    }

    #[test]
    fn plan_environment_always_has_fleet_vars(spec in arb_fleet_spec(1, 3)) {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let plan = launcher.plan(&spec).unwrap();
        for slot in &plan.slots {
            prop_assert!(slot.environment.contains_key("FT_FLEET_NAME"));
            prop_assert!(slot.environment.contains_key("FT_SLOT_INDEX"));
            prop_assert!(slot.environment.contains_key("FT_MIX_ENTRY"));
            prop_assert_eq!(
                slot.environment.get("FT_FLEET_NAME").unwrap(),
                &spec.name
            );
        }
    }

    #[test]
    fn plan_phases_cover_all_slots(
        spec in arb_fleet_spec(1, 5)
    ) {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let plan = launcher.plan(&spec).unwrap();
        let mut all_phase_slots: Vec<u32> = plan
            .phases
            .iter()
            .flat_map(|p| p.slot_indices.iter().copied())
            .collect();
        all_phase_slots.sort();
        all_phase_slots.dedup();
        prop_assert_eq!(
            all_phase_slots.len(),
            plan.slots.len(),
            "phases must cover all slots"
        );
    }

    #[test]
    fn plan_generation_propagated(
        spec in arb_fleet_spec(1, 3)
    ) {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let plan = launcher.plan(&spec).unwrap();
        prop_assert_eq!(plan.generation, spec.generation);
        for slot in &plan.slots {
            prop_assert_eq!(slot.lifecycle_identity.generation, spec.generation);
        }
    }

    #[test]
    fn execute_outcome_status_correct(spec in arb_fleet_spec(1, 5)) {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let mut lifecycle = LifecycleRegistry::new();
        let outcome = launcher.launch(&spec, &mut lifecycle).unwrap();

        // Fresh registry with no pre-existing entities: all should succeed
        prop_assert_eq!(outcome.status, FleetLaunchStatus::Complete);
        prop_assert_eq!(outcome.successful_slots, outcome.total_slots);
        prop_assert_eq!(outcome.failed_slots, 0);
    }

    #[test]
    fn execute_entity_counts_correct(spec in arb_fleet_spec(1, 5)) {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let mut lifecycle = LifecycleRegistry::new();
        let outcome = launcher.launch(&spec, &mut lifecycle).unwrap();
        let n = outcome.total_slots as usize;
        // 1 session + 1 window + n panes + n agents = n*2 + 2
        prop_assert_eq!(lifecycle.len(), n * 2 + 2);
    }

    #[test]
    fn execute_all_outcomes_registered(spec in arb_fleet_spec(1, 5)) {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let mut lifecycle = LifecycleRegistry::new();
        let outcome = launcher.launch(&spec, &mut lifecycle).unwrap();
        for so in &outcome.slot_outcomes {
            prop_assert_eq!(so.status, SlotStatus::Registered);
            prop_assert!(so.error.is_none());
        }
    }

    #[test]
    fn plan_empty_mix_always_fails(strategy in arb_startup_strategy()) {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let spec = FleetSpec {
            name: "empty".to_string(),
            description: None,
            workspace_id: "ws".to_string(),
            domain: "local".to_string(),
            mix: vec![],
            total_panes: 0,
            fleet_template: None,
            working_directory: None,
            startup_strategy: strategy,
            generation: 1,
            tags: vec![],
        };
        let result = launcher.plan(&spec);
        prop_assert_eq!(result.unwrap_err(), FleetLaunchError::EmptyMix);
    }

    #[test]
    fn plan_phased_phases_sorted(
        mix in proptest::collection::vec(
            (arb_mix_entry(), arb_profile_role()).prop_map(|(mut entry, role)| {
                entry.role = Some(role);
                entry
            }),
            2..=5
        )
    ) {
        let spec = FleetSpec {
            name: "phased".to_string(),
            description: None,
            workspace_id: "ws".to_string(),
            domain: "local".to_string(),
            mix,
            total_panes: 0,
            fleet_template: None,
            working_directory: None,
            startup_strategy: StartupStrategy::Phased,
            generation: 1,
            tags: vec![],
        };
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let plan = launcher.plan(&spec).unwrap();
        let phase_indices: Vec<u32> = plan.phases.iter().map(|p| p.index).collect();
        prop_assert!(
            phase_indices.windows(2).all(|w| w[0] <= w[1]),
            "phases must be sorted by index"
        );
    }

    #[test]
    fn plan_parallel_has_single_phase(spec in arb_fleet_spec(1, 5).prop_filter(
        "parallel only",
        |s| s.startup_strategy == StartupStrategy::Parallel
    )) {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let plan = launcher.plan(&spec).unwrap();
        prop_assert_eq!(plan.phases.len(), 1, "parallel strategy should have exactly 1 phase");
    }

    #[test]
    fn plan_mix_entry_index_valid(spec in arb_fleet_spec(1, 5)) {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let plan = launcher.plan(&spec).unwrap();
        for slot in &plan.slots {
            prop_assert!(
                slot.mix_entry_index < spec.mix.len(),
                "mix_entry_index {} out of bounds (len={})",
                slot.mix_entry_index,
                spec.mix.len()
            );
        }
    }

    #[test]
    fn execute_completed_at_after_planned_at(spec in arb_fleet_spec(1, 3)) {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let plan = launcher.plan(&spec).unwrap();
        let mut lifecycle = LifecycleRegistry::new();
        let outcome = launcher.execute(&plan, &mut lifecycle);
        prop_assert!(outcome.completed_at >= plan.planned_at);
    }

    #[test]
    fn plan_domain_and_workspace_propagated(spec in arb_fleet_spec(1, 3)) {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let plan = launcher.plan(&spec).unwrap();
        prop_assert_eq!(&plan.workspace_id, &spec.workspace_id);
        prop_assert_eq!(&plan.domain, &spec.domain);
        for slot in &plan.slots {
            prop_assert_eq!(&slot.lifecycle_identity.workspace_id, &spec.workspace_id);
            prop_assert_eq!(&slot.lifecycle_identity.domain, &spec.domain);
        }
    }

    // -------------------------------------------------------------------
    // Weighted allocation properties
    // -------------------------------------------------------------------

    #[test]
    fn weighted_allocation_sum_equals_total(
        total in 1u32..50u32,
        mix in proptest::collection::vec(arb_mix_entry(), 1..=5),
    ) {
        let spec = FleetSpec {
            name: "alloc-sum".to_string(),
            description: None,
            workspace_id: "ws".to_string(),
            domain: "local".to_string(),
            mix,
            total_panes: total,
            fleet_template: None,
            working_directory: None,
            startup_strategy: StartupStrategy::Parallel,
            generation: 1,
            tags: vec![],
        };
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let plan = launcher.plan(&spec).unwrap();
        prop_assert_eq!(
            plan.slots.len() as u32, total,
            "allocated slots must equal requested total"
        );
    }

    #[test]
    fn weighted_allocation_respects_relative_proportions(
        total in 10u32..40u32,
    ) {
        let mix = vec![
            AgentMixEntry {
                program: "a".to_string(),
                model: None,
                weight: 3,
                profile: None,
                task_template: None,
                environment: HashMap::new(),
                role: None,
            },
            AgentMixEntry {
                program: "b".to_string(),
                model: None,
                weight: 1,
                profile: None,
                task_template: None,
                environment: HashMap::new(),
                role: None,
            },
        ];
        let spec = FleetSpec {
            name: "proportion".to_string(),
            description: None,
            workspace_id: "ws".to_string(),
            domain: "local".to_string(),
            mix,
            total_panes: total,
            fleet_template: None,
            working_directory: None,
            startup_strategy: StartupStrategy::Parallel,
            generation: 1,
            tags: vec![],
        };
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let plan = launcher.plan(&spec).unwrap();
        let a_count = plan.slots.iter().filter(|s| s.mix_entry_index == 0).count() as u32;
        let b_count = plan.slots.iter().filter(|s| s.mix_entry_index == 1).count() as u32;
        // With 3:1 ratio, 'a' must always get at least as many as 'b'
        prop_assert!(a_count >= b_count, "3:1 weight: a={a_count} must >= b={b_count}");
    }

    // -------------------------------------------------------------------
    // Sequential halt on conflict
    // -------------------------------------------------------------------

    #[test]
    fn sequential_halt_skips_remaining_on_conflict(
        n_slots in 3u32..8u32,
    ) {
        let mix = vec![AgentMixEntry {
            program: "test".to_string(),
            model: None,
            weight: n_slots,
            profile: None,
            task_template: None,
            environment: HashMap::new(),
            role: None,
        }];
        let spec = FleetSpec {
            name: "seq-conflict".to_string(),
            description: None,
            workspace_id: "ws".to_string(),
            domain: "local".to_string(),
            mix,
            total_panes: 0,
            fleet_template: None,
            working_directory: None,
            startup_strategy: StartupStrategy::Sequential,
            generation: 1,
            tags: vec![],
        };
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let plan = launcher.plan(&spec).unwrap();

        // Pre-register the second slot's pane identity to trigger conflict
        let mut lifecycle = LifecycleRegistry::new();
        if plan.slots.len() >= 2 {
            lifecycle.register_entity(
                plan.slots[1].lifecycle_identity.clone(),
                LifecycleState::Pane(MuxPaneLifecycleState::Running),
                0,
            ).ok();

            let outcome = launcher.execute(&plan, &mut lifecycle);

            // First slot succeeds, second fails (conflict), rest skipped
            prop_assert_eq!(outcome.slot_outcomes[0].status, SlotStatus::Registered);
            prop_assert_eq!(outcome.slot_outcomes[1].status, SlotStatus::Failed);
            for so in &outcome.slot_outcomes[2..] {
                prop_assert_eq!(so.status, SlotStatus::Skipped,
                    "slots after sequential failure must be skipped");
            }
            prop_assert_eq!(outcome.status, FleetLaunchStatus::Partial);
            prop_assert_eq!(outcome.successful_slots, 1);
        }
    }

    // -------------------------------------------------------------------
    // Subsystem integration: durable state checkpoint
    // -------------------------------------------------------------------

    #[test]
    fn launch_with_durable_state_creates_checkpoint(spec in arb_fleet_spec(1, 3)) {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let mut lifecycle = LifecycleRegistry::new();
        let mut durable = DurableStateManager::new();

        let outcome = launcher.launch_with_subsystems(
            &spec, &mut lifecycle, Some(&mut durable), None
        ).unwrap();

        prop_assert!(outcome.pre_launch_checkpoint.is_some(),
            "durable state integration must produce a checkpoint");
        let checkpoint_id = outcome.pre_launch_checkpoint.unwrap();
        prop_assert!(checkpoint_id > 0, "checkpoint ID must be positive");
    }

    // -------------------------------------------------------------------
    // Subsystem integration: command router bootstrap dispatch
    // -------------------------------------------------------------------

    #[test]
    fn launch_with_router_dispatches_bootstrap_commands(
        n_slots in 1u32..4u32,
    ) {
        let mix = vec![AgentMixEntry {
            program: "test".to_string(),
            model: None,
            weight: n_slots,
            profile: None,
            task_template: None,
            environment: HashMap::new(),
            role: None,
        }];
        let spec = FleetSpec {
            name: "bootstrap-test".to_string(),
            description: None,
            workspace_id: "ws".to_string(),
            domain: "local".to_string(),
            mix,
            total_panes: 0,
            fleet_template: None,
            working_directory: None,
            startup_strategy: StartupStrategy::Parallel,
            generation: 1,
            tags: vec![],
        };
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let mut lifecycle = LifecycleRegistry::new();
        let mut router = CommandRouter::new();

        let outcome = launcher.launch_with_subsystems(
            &spec, &mut lifecycle, None, Some(&mut router),
        ).unwrap();

        // Bootstrap dispatches are recorded (may be 0 per slot if no bootstrap commands)
        for &(slot_idx, cmd_count) in &outcome.bootstrap_dispatches {
            prop_assert!(slot_idx < n_slots,
                "dispatch slot index must be within range");
            // cmd_count may be 0 — just verify it's not absurdly large
            prop_assert!(cmd_count < 100,
                "bootstrap commands per slot should be reasonable");
        }
        // Router audit log should have entries for any dispatched bootstrap commands
        let total_dispatched: usize = outcome.bootstrap_dispatches.iter().map(|(_, c)| c).sum();
        prop_assert_eq!(router.audit_log().len(), total_dispatched);
    }

    // -------------------------------------------------------------------
    // Outcome status classification
    // -------------------------------------------------------------------

    #[test]
    fn complete_outcome_has_zero_failures(spec in arb_fleet_spec(1, 5)) {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let mut lifecycle = LifecycleRegistry::new();
        let outcome = launcher.launch(&spec, &mut lifecycle).unwrap();

        if outcome.status == FleetLaunchStatus::Complete {
            prop_assert_eq!(outcome.failed_slots, 0);
            prop_assert_eq!(outcome.successful_slots, outcome.total_slots);
        }
    }

    #[test]
    fn outcome_slot_count_sum_invariant(spec in arb_fleet_spec(1, 5)) {
        let reg = test_registry();
        let launcher = FleetLauncher::new(&reg);
        let mut lifecycle = LifecycleRegistry::new();
        let outcome = launcher.launch(&spec, &mut lifecycle).unwrap();

        // successful + failed always <= total (skipped slots aren't counted in either)
        prop_assert!(outcome.successful_slots + outcome.failed_slots <= outcome.total_slots);
        // slot_outcomes len always equals total
        prop_assert_eq!(outcome.slot_outcomes.len() as u32, outcome.total_slots);
    }
}
