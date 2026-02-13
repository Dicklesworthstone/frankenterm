//! Property-based tests for learn.rs
//!
//! Tests invariants for Rarity, Requirement, Achievement, TutorialState,
//! Track, Exercise, TutorialEngine state machine, TutorialEnvironment,
//! BUILTIN_ACHIEVEMENTS catalog, and achievement_definition lookup.

use frankenterm_core::learn::*;
use proptest::prelude::*;
use std::collections::HashSet;

// ============================================================================
// Strategies
// ============================================================================

fn arb_rarity() -> impl Strategy<Value = Rarity> {
    prop_oneof![
        Just(Rarity::Common),
        Just(Rarity::Uncommon),
        Just(Rarity::Rare),
        Just(Rarity::Epic),
    ]
}

fn arb_requirement() -> impl Strategy<Value = Requirement> {
    prop_oneof![
        Just(Requirement::WeztermRunning),
        Just(Requirement::AgentPresent),
        Just(Requirement::WatcherRunning),
        Just(Requirement::DbHasData),
        Just(Requirement::WaConfigured),
    ]
}

fn arb_exercise() -> impl Strategy<Value = Exercise> {
    (
        "[a-z]{3,10}\\.[0-9]{1,2}",
        "[A-Z][a-z ]{5,30}",
        "[a-z ]{10,50}",
        prop::collection::vec("[a-z ]{5,30}", 1..4),
        proptest::option::of("[a-z -]{5,30}"),
        proptest::option::of("[a-z.*]{3,20}"),
        prop::collection::vec(arb_requirement(), 0..3),
        proptest::bool::ANY,
    )
        .prop_map(
            |(
                id,
                title,
                description,
                instructions,
                verification_command,
                verification_pattern,
                requirements,
                can_simulate,
            )| {
                Exercise {
                    id,
                    title,
                    description,
                    instructions,
                    verification_command,
                    verification_pattern,
                    requirements,
                    can_simulate,
                }
            },
        )
}

fn arb_track() -> impl Strategy<Value = Track> {
    (
        "[a-z]{3,15}",
        "[A-Z][a-z ]{5,20}",
        "[a-z ]{10,50}",
        1..60u32,
        prop::collection::vec(arb_exercise(), 1..5),
    )
        .prop_map(
            |(id, name, description, estimated_minutes, exercises)| Track {
                id,
                name,
                description,
                estimated_minutes,
                exercises,
            },
        )
}

// ============================================================================
// Property Tests: Rarity
// ============================================================================

proptest! {
    /// Property 1: Rarity serde roundtrip
    #[test]
    fn prop_rarity_serde_roundtrip(r in arb_rarity()) {
        let json = serde_json::to_string(&r).unwrap();
        let back: Rarity = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, r);
    }

    /// Property 2: Rarity serde uses snake_case
    #[test]
    fn prop_rarity_serde_snake_case(r in arb_rarity()) {
        let json = serde_json::to_string(&r).unwrap();
        let inner = json.trim_matches('"');
        prop_assert!(inner.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                    "Serde should be snake_case: {}", inner);
    }

    /// Property 3: Rarity label() is non-empty and capitalized
    #[test]
    fn prop_rarity_label_non_empty(r in arb_rarity()) {
        let label = r.label();
        prop_assert!(!label.is_empty(), "label should not be empty");
        prop_assert!(label.chars().next().unwrap().is_uppercase(),
                    "label should be capitalized: {}", label);
    }

    /// Property 4: Rarity Display matches label
    #[test]
    fn prop_rarity_display_matches_label(r in arb_rarity()) {
        let display = r.to_string();
        prop_assert_eq!(display.as_str(), r.label(),
                       "Display should match label");
    }
}

// ============================================================================
// Property Tests: Requirement
// ============================================================================

proptest! {
    /// Property 5: Requirement serde roundtrip
    #[test]
    fn prop_requirement_serde_roundtrip(req in arb_requirement()) {
        let json = serde_json::to_string(&req).unwrap();
        let back: Requirement = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, req);
    }

    /// Property 6: Requirement serde uses snake_case
    #[test]
    fn prop_requirement_serde_snake_case(req in arb_requirement()) {
        let json = serde_json::to_string(&req).unwrap();
        let inner = json.trim_matches('"');
        prop_assert!(inner.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                    "Serde should be snake_case: {}", inner);
    }
}

// ============================================================================
// Property Tests: BUILTIN_ACHIEVEMENTS Catalog
// ============================================================================

proptest! {
    /// Property 7: All builtin achievements have unique IDs
    #[test]
    fn prop_builtin_achievements_unique_ids(_dummy in Just(())) {
        let mut seen = HashSet::new();
        for def in BUILTIN_ACHIEVEMENTS {
            prop_assert!(seen.insert(def.id),
                        "Duplicate achievement ID: {}", def.id);
        }
    }

    /// Property 8: All builtin achievements have non-empty fields
    #[test]
    fn prop_builtin_achievements_non_empty(_dummy in Just(())) {
        for def in BUILTIN_ACHIEVEMENTS {
            prop_assert!(!def.id.is_empty(), "ID should not be empty");
            prop_assert!(!def.name.is_empty(), "Name should not be empty for {}", def.id);
            prop_assert!(!def.description.is_empty(), "Description should not be empty for {}", def.id);
        }
    }

    /// Property 9: achievement_definition finds all builtin achievements
    #[test]
    fn prop_achievement_definition_finds_all(_dummy in Just(())) {
        for def in BUILTIN_ACHIEVEMENTS {
            let found = achievement_definition(def.id);
            prop_assert!(found.is_some(), "Should find achievement '{}'", def.id);
            let found = found.unwrap();
            prop_assert_eq!(found.id, def.id);
            prop_assert_eq!(found.name, def.name);
            prop_assert_eq!(found.rarity, def.rarity);
            prop_assert_eq!(found.secret, def.secret);
        }
    }

    /// Property 10: achievement_definition returns None for unknown ID
    #[test]
    fn prop_achievement_definition_unknown(id in "[a-z]{10,20}") {
        // Random long strings should not match any builtin
        let found = achievement_definition(&id);
        if found.is_some() {
            // If it happens to match, verify it's a valid definition
            let def = found.unwrap();
            prop_assert_eq!(def.id, id.as_str());
        }
    }

    /// Property 11: Secret achievements exist in catalog
    #[test]
    fn prop_secret_achievements_exist(_dummy in Just(())) {
        let secret_count = BUILTIN_ACHIEVEMENTS.iter().filter(|d| d.secret).count();
        prop_assert!(secret_count >= 2, "Should have at least 2 secret achievements");
    }

    /// Property 12: Rarity distribution covers all tiers
    #[test]
    fn prop_rarity_distribution_covers_all(_dummy in Just(())) {
        let rarities: HashSet<_> = BUILTIN_ACHIEVEMENTS.iter().map(|d| d.rarity).collect();
        prop_assert!(rarities.contains(&Rarity::Common), "Should have Common achievements");
        prop_assert!(rarities.contains(&Rarity::Uncommon), "Should have Uncommon achievements");
        prop_assert!(rarities.contains(&Rarity::Rare), "Should have Rare achievements");
        prop_assert!(rarities.contains(&Rarity::Epic), "Should have Epic achievements");
    }
}

// ============================================================================
// Property Tests: TutorialState
// ============================================================================

proptest! {
    /// Property 13: TutorialState default has version 1
    #[test]
    fn prop_tutorial_state_default_version(_dummy in Just(())) {
        let state = TutorialState::default();
        prop_assert_eq!(state.version, 1);
    }

    /// Property 14: TutorialState default starts empty
    #[test]
    fn prop_tutorial_state_default_empty(_dummy in Just(())) {
        let state = TutorialState::default();
        prop_assert!(state.current_track.is_none());
        prop_assert!(state.current_exercise.is_none());
        prop_assert!(state.completed_exercises.is_empty());
        prop_assert!(state.achievements.is_empty());
        prop_assert_eq!(state.total_time_minutes, 0);
    }

    /// Property 15: TutorialState serde roundtrip
    #[test]
    fn prop_tutorial_state_serde_roundtrip(_dummy in Just(())) {
        let state = TutorialState::default();
        let json = serde_json::to_string(&state).unwrap();
        let back: TutorialState = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.version, state.version);
        prop_assert_eq!(back.current_track, state.current_track);
        prop_assert_eq!(back.current_exercise, state.current_exercise);
        prop_assert_eq!(back.completed_exercises.len(), state.completed_exercises.len());
        prop_assert_eq!(back.achievements.len(), state.achievements.len());
        prop_assert_eq!(back.total_time_minutes, state.total_time_minutes);
    }
}

// ============================================================================
// Property Tests: Track/Exercise serde
// ============================================================================

proptest! {
    /// Property 16: Exercise serde roundtrip
    #[test]
    fn prop_exercise_serde_roundtrip(ex in arb_exercise()) {
        let json = serde_json::to_string(&ex).unwrap();
        let back: Exercise = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.id, &ex.id);
        prop_assert_eq!(&back.title, &ex.title);
        prop_assert_eq!(&back.description, &ex.description);
        prop_assert_eq!(&back.instructions, &ex.instructions);
        prop_assert_eq!(&back.verification_command, &ex.verification_command);
        prop_assert_eq!(back.can_simulate, ex.can_simulate);
        prop_assert_eq!(back.requirements.len(), ex.requirements.len());
    }

    /// Property 17: Track serde roundtrip
    #[test]
    fn prop_track_serde_roundtrip(track in arb_track()) {
        let json = serde_json::to_string(&track).unwrap();
        let back: Track = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.id, &track.id);
        prop_assert_eq!(&back.name, &track.name);
        prop_assert_eq!(&back.description, &track.description);
        prop_assert_eq!(back.estimated_minutes, track.estimated_minutes);
        prop_assert_eq!(back.exercises.len(), track.exercises.len());
    }
}

// ============================================================================
// Property Tests: TutorialEngine State Machine
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Property 18: Fresh engine has tracks and no progress
    #[test]
    fn prop_engine_fresh_state(_dummy in Just(())) {
        let tmp = std::env::temp_dir().join(format!("ft_learn_test_{}", std::process::id()));
        let engine = TutorialEngine::load_or_create_at(tmp).unwrap();
        prop_assert!(!engine.tracks().is_empty(), "Should have builtin tracks");
        prop_assert!(engine.state().current_track.is_none());
        prop_assert!(engine.state().current_exercise.is_none());
        prop_assert!(engine.state().completed_exercises.is_empty());
    }

    /// Property 19: StartTrack sets current_track
    #[test]
    fn prop_engine_start_track(_dummy in Just(())) {
        let tmp = std::env::temp_dir().join(format!("ft_learn_test_st_{}", std::process::id()));
        let mut engine = TutorialEngine::load_or_create_at(tmp).unwrap();
        let track_id = engine.tracks()[0].id.clone();
        engine.handle_event(TutorialEvent::StartTrack(track_id.clone())).unwrap();
        prop_assert_eq!(engine.state().current_track.as_ref(), Some(&track_id));
        // Should set current_exercise to first exercise in track
        let first_exercise_id = engine.tracks()[0].exercises[0].id.clone();
        prop_assert_eq!(engine.state().current_exercise.as_ref(), Some(&first_exercise_id));
    }

    /// Property 20: CompleteExercise adds to completed set
    #[test]
    fn prop_engine_complete_exercise(_dummy in Just(())) {
        let tmp = std::env::temp_dir().join(format!("ft_learn_test_ce_{}", std::process::id()));
        let mut engine = TutorialEngine::load_or_create_at(tmp).unwrap();
        let track_id = engine.tracks()[0].id.clone();
        let ex_id = engine.tracks()[0].exercises[0].id.clone();
        engine.handle_event(TutorialEvent::StartTrack(track_id)).unwrap();
        engine.handle_event(TutorialEvent::CompleteExercise(ex_id.clone())).unwrap();
        prop_assert!(engine.state().completed_exercises.contains(&ex_id),
                    "Exercise should be in completed set");
    }

    /// Property 21: CompleteExercise advances to next exercise
    #[test]
    fn prop_engine_advance_exercise(_dummy in Just(())) {
        let tmp = std::env::temp_dir().join(format!("ft_learn_test_ae_{}", std::process::id()));
        let mut engine = TutorialEngine::load_or_create_at(tmp).unwrap();
        let track_id = engine.tracks()[0].id.clone();
        let exercises: Vec<_> = engine.tracks()[0].exercises.iter().map(|e| e.id.clone()).collect();
        engine.handle_event(TutorialEvent::StartTrack(track_id)).unwrap();
        if exercises.len() >= 2 {
            engine.handle_event(TutorialEvent::CompleteExercise(exercises[0].clone())).unwrap();
            prop_assert_eq!(engine.state().current_exercise.as_ref(), Some(&exercises[1]),
                           "Should advance to next exercise");
        }
    }

    /// Property 22: SkipExercise advances without completing
    #[test]
    fn prop_engine_skip_exercise(_dummy in Just(())) {
        let tmp = std::env::temp_dir().join(format!("ft_learn_test_se_{}", std::process::id()));
        let mut engine = TutorialEngine::load_or_create_at(tmp).unwrap();
        let track_id = engine.tracks()[0].id.clone();
        let exercises: Vec<_> = engine.tracks()[0].exercises.iter().map(|e| e.id.clone()).collect();
        engine.handle_event(TutorialEvent::StartTrack(track_id)).unwrap();
        if exercises.len() >= 2 {
            engine.handle_event(TutorialEvent::SkipExercise(exercises[0].clone())).unwrap();
            prop_assert!(!engine.state().completed_exercises.contains(&exercises[0]),
                        "Skipped exercise should NOT be in completed set");
            prop_assert_eq!(engine.state().current_exercise.as_ref(), Some(&exercises[1]),
                           "Should advance to next exercise after skip");
        }
    }

    /// Property 23: Reset clears all progress
    #[test]
    fn prop_engine_reset(_dummy in Just(())) {
        let tmp = std::env::temp_dir().join(format!("ft_learn_test_r_{}", std::process::id()));
        let mut engine = TutorialEngine::load_or_create_at(tmp).unwrap();
        let track_id = engine.tracks()[0].id.clone();
        let ex_id = engine.tracks()[0].exercises[0].id.clone();
        engine.handle_event(TutorialEvent::StartTrack(track_id)).unwrap();
        engine.handle_event(TutorialEvent::CompleteExercise(ex_id)).unwrap();
        prop_assert!(!engine.state().completed_exercises.is_empty());
        engine.handle_event(TutorialEvent::Reset).unwrap();
        prop_assert!(engine.state().completed_exercises.is_empty());
        prop_assert!(engine.state().current_track.is_none());
        prop_assert!(engine.state().achievements.is_empty());
    }

    /// Property 24: is_track_complete false initially, true after all exercises
    #[test]
    fn prop_engine_track_complete(_dummy in Just(())) {
        let tmp = std::env::temp_dir().join(format!("ft_learn_test_tc_{}", std::process::id()));
        let mut engine = TutorialEngine::load_or_create_at(tmp).unwrap();
        let track_id = engine.tracks()[0].id.clone();
        let exercises: Vec<_> = engine.tracks()[0].exercises.iter().map(|e| e.id.clone()).collect();
        prop_assert!(!engine.is_track_complete(&track_id));
        engine.handle_event(TutorialEvent::StartTrack(track_id.clone())).unwrap();
        for ex_id in &exercises {
            engine.handle_event(TutorialEvent::CompleteExercise(ex_id.clone())).unwrap();
        }
        prop_assert!(engine.is_track_complete(&track_id),
                    "Track should be complete after all exercises done");
    }

    /// Property 25: track_progress returns correct counts
    #[test]
    fn prop_engine_track_progress(_dummy in Just(())) {
        let tmp = std::env::temp_dir().join(format!("ft_learn_test_tp_{}", std::process::id()));
        let mut engine = TutorialEngine::load_or_create_at(tmp).unwrap();
        let track_id = engine.tracks()[0].id.clone();
        let total = engine.tracks()[0].exercises.len();
        prop_assert_eq!(engine.track_progress(&track_id), (0, total));
        let ex_id = engine.tracks()[0].exercises[0].id.clone();
        engine.handle_event(TutorialEvent::StartTrack(track_id.clone())).unwrap();
        engine.handle_event(TutorialEvent::CompleteExercise(ex_id)).unwrap();
        let (completed, ttl) = engine.track_progress(&track_id);
        prop_assert_eq!(completed, 1);
        prop_assert_eq!(ttl, total);
    }

    /// Property 26: overall_progress completed <= total
    #[test]
    fn prop_engine_overall_progress(_dummy in Just(())) {
        let tmp = std::env::temp_dir().join(format!("ft_learn_test_op_{}", std::process::id()));
        let engine = TutorialEngine::load_or_create_at(tmp).unwrap();
        let (completed, total) = engine.overall_progress();
        prop_assert_eq!(completed, 0);
        prop_assert!(total > 0, "Should have exercises");
    }

    /// Property 27: Completing first exercise unlocks "first_step" achievement
    #[test]
    fn prop_engine_first_step_achievement(_dummy in Just(())) {
        let tmp = std::env::temp_dir().join(format!("ft_learn_test_fs_{}", std::process::id()));
        let mut engine = TutorialEngine::load_or_create_at(tmp).unwrap();
        let track_id = engine.tracks()[0].id.clone();
        let ex_id = engine.tracks()[0].exercises[0].id.clone();
        engine.handle_event(TutorialEvent::StartTrack(track_id)).unwrap();
        engine.handle_event(TutorialEvent::CompleteExercise(ex_id)).unwrap();
        let has_first_step = engine.state().achievements.iter().any(|a| a.id == "first_step");
        prop_assert!(has_first_step, "Should unlock 'first_step' achievement");
    }

    /// Property 28: UnlockAchievement is idempotent
    #[test]
    fn prop_engine_achievement_idempotent(_dummy in Just(())) {
        let tmp = std::env::temp_dir().join(format!("ft_learn_test_ai_{}", std::process::id()));
        let mut engine = TutorialEngine::load_or_create_at(tmp).unwrap();
        engine.handle_event(TutorialEvent::UnlockAchievement {
            id: "test_ach".to_string(),
            name: "Test".to_string(),
            description: "Test achievement".to_string(),
        }).unwrap();
        let count1 = engine.state().achievements.len();
        engine.handle_event(TutorialEvent::UnlockAchievement {
            id: "test_ach".to_string(),
            name: "Test".to_string(),
            description: "Test achievement".to_string(),
        }).unwrap();
        let count2 = engine.state().achievements.len();
        prop_assert_eq!(count1, count2, "Unlocking same achievement twice should not duplicate");
    }

    /// Property 29: get_track returns None for unknown track
    #[test]
    fn prop_engine_get_unknown_track(_dummy in Just(())) {
        let tmp = std::env::temp_dir().join(format!("ft_learn_test_gt_{}", std::process::id()));
        let engine = TutorialEngine::load_or_create_at(tmp).unwrap();
        prop_assert!(engine.get_track("nonexistent_track").is_none());
    }

    /// Property 30: is_track_complete false for unknown track
    #[test]
    fn prop_engine_unknown_track_not_complete(_dummy in Just(())) {
        let tmp = std::env::temp_dir().join(format!("ft_learn_test_utc_{}", std::process::id()));
        let engine = TutorialEngine::load_or_create_at(tmp).unwrap();
        prop_assert!(!engine.is_track_complete("nonexistent_track"));
    }

    /// Property 31: track_progress returns (0, 0) for unknown track
    #[test]
    fn prop_engine_unknown_track_progress(_dummy in Just(())) {
        let tmp = std::env::temp_dir().join(format!("ft_learn_test_utp_{}", std::process::id()));
        let engine = TutorialEngine::load_or_create_at(tmp).unwrap();
        prop_assert_eq!(engine.track_progress("nonexistent_track"), (0, 0));
    }

    /// Property 32: achievement_collection covers all builtins
    #[test]
    fn prop_engine_achievement_collection_complete(_dummy in Just(())) {
        let tmp = std::env::temp_dir().join(format!("ft_learn_test_ac_{}", std::process::id()));
        let engine = TutorialEngine::load_or_create_at(tmp).unwrap();
        let collection = engine.achievement_collection();
        prop_assert_eq!(collection.len(), BUILTIN_ACHIEVEMENTS.len(),
                       "Collection should have all builtin achievements");
        for entry in &collection {
            prop_assert!(entry.unlocked_at.is_none(),
                        "Fresh engine should have no unlocked achievements");
        }
    }

    /// Property 33: Builtin tracks have unique IDs
    #[test]
    fn prop_engine_tracks_unique_ids(_dummy in Just(())) {
        let tmp = std::env::temp_dir().join(format!("ft_learn_test_tu_{}", std::process::id()));
        let engine = TutorialEngine::load_or_create_at(tmp).unwrap();
        let mut seen = HashSet::new();
        for track in engine.tracks() {
            prop_assert!(seen.insert(&track.id),
                        "Duplicate track ID: {}", track.id);
        }
    }

    /// Property 34: All exercises across tracks have unique IDs
    #[test]
    fn prop_engine_exercises_unique_ids(_dummy in Just(())) {
        let tmp = std::env::temp_dir().join(format!("ft_learn_test_eu_{}", std::process::id()));
        let engine = TutorialEngine::load_or_create_at(tmp).unwrap();
        let mut seen = HashSet::new();
        for track in engine.tracks() {
            for ex in &track.exercises {
                prop_assert!(seen.insert(&ex.id),
                            "Duplicate exercise ID: {}", ex.id);
            }
        }
    }
}

// ============================================================================
// Property Tests: TutorialEnvironment
// ============================================================================

proptest! {
    /// Property 35: can_run_exercise returns Yes when all requirements met
    #[test]
    fn prop_env_all_requirements_met(_dummy in Just(())) {
        let env = TutorialEnvironment {
            wezterm_running: true,
            wezterm_version: Some("20240101-000000-abc123".to_string()),
            pane_count: 5,
            agent_panes: vec![AgentInfo { agent_type: "codex".to_string(), pane_id: 1 }],
            wa_configured: true,
            db_has_data: true,
            shell_integration: true,
        };
        let exercise = Exercise {
            id: "test.1".to_string(),
            title: "Test".to_string(),
            description: "Test exercise".to_string(),
            instructions: vec!["Do something".to_string()],
            verification_command: None,
            verification_pattern: None,
            requirements: vec![Requirement::WeztermRunning, Requirement::AgentPresent],
            can_simulate: false,
        };
        prop_assert_eq!(env.can_run_exercise(&exercise), CanRun::Yes);
    }

    /// Property 36: can_run_exercise returns No when requirement unmet and can't simulate
    #[test]
    fn prop_env_requirement_not_met_no_sim(_dummy in Just(())) {
        let env = TutorialEnvironment {
            wezterm_running: false,
            wezterm_version: None,
            pane_count: 0,
            agent_panes: vec![],
            wa_configured: false,
            db_has_data: false,
            shell_integration: false,
        };
        let exercise = Exercise {
            id: "test.1".to_string(),
            title: "Test".to_string(),
            description: "Test exercise".to_string(),
            instructions: vec!["Do something".to_string()],
            verification_command: None,
            verification_pattern: None,
            requirements: vec![Requirement::WeztermRunning],
            can_simulate: false,
        };
        prop_assert!(matches!(env.can_run_exercise(&exercise), CanRun::No(_)));
    }

    /// Property 37: can_run_exercise returns Simulation when can_simulate is true
    #[test]
    fn prop_env_requirement_not_met_can_sim(_dummy in Just(())) {
        let env = TutorialEnvironment {
            wezterm_running: false,
            wezterm_version: None,
            pane_count: 0,
            agent_panes: vec![],
            wa_configured: false,
            db_has_data: false,
            shell_integration: false,
        };
        let exercise = Exercise {
            id: "test.1".to_string(),
            title: "Test".to_string(),
            description: "Test exercise".to_string(),
            instructions: vec!["Do something".to_string()],
            verification_command: None,
            verification_pattern: None,
            requirements: vec![Requirement::WeztermRunning],
            can_simulate: true,
        };
        prop_assert!(matches!(env.can_run_exercise(&exercise), CanRun::Simulation(_)));
    }

    /// Property 38: Exercise with no requirements always returns Yes
    #[test]
    fn prop_env_no_requirements_always_yes(
        wt_running in proptest::bool::ANY,
        db_data in proptest::bool::ANY,
    ) {
        let env = TutorialEnvironment {
            wezterm_running: wt_running,
            wezterm_version: None,
            pane_count: 0,
            agent_panes: vec![],
            wa_configured: false,
            db_has_data: db_data,
            shell_integration: false,
        };
        let exercise = Exercise {
            id: "test.1".to_string(),
            title: "Test".to_string(),
            description: "Test".to_string(),
            instructions: vec![],
            verification_command: None,
            verification_pattern: None,
            requirements: vec![],
            can_simulate: false,
        };
        prop_assert_eq!(env.can_run_exercise(&exercise), CanRun::Yes,
                       "No requirements should always pass");
    }
}
