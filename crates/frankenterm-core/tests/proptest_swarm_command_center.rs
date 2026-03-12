//! Property tests for swarm_command_center module (ft-3681t.9.2).
//!
//! Covers serde roundtrips, operator level ordering, fuzzy match scoring,
//! action availability logic, command palette search consistency, live view
//! health summary arithmetic, latency budget checks, and standard factories.

use frankenterm_core::swarm_command_center::*;
use proptest::prelude::*;

// =============================================================================
// Strategies
// =============================================================================

fn arb_operator_level() -> impl Strategy<Value = OperatorLevel> {
    prop_oneof![
        Just(OperatorLevel::Observer),
        Just(OperatorLevel::Operator),
        Just(OperatorLevel::SeniorOperator),
        Just(OperatorLevel::Admin),
    ]
}

fn arb_action_category() -> impl Strategy<Value = ActionCategory> {
    prop_oneof![
        Just(ActionCategory::FleetControl),
        Just(ActionCategory::PaneManagement),
        Just(ActionCategory::AgentLifecycle),
        Just(ActionCategory::PolicySafety),
        Just(ActionCategory::Diagnostics),
        Just(ActionCategory::SessionManagement),
        Just(ActionCategory::Navigation),
        Just(ActionCategory::Configuration),
    ]
}

fn arb_pane_health() -> impl Strategy<Value = PaneHealth> {
    prop_oneof![
        Just(PaneHealth::Healthy),
        Just(PaneHealth::Degraded),
        Just(PaneHealth::Unhealthy),
        Just(PaneHealth::Stopped),
    ]
}

fn arb_key_action() -> impl Strategy<Value = KeyAction> {
    prop_oneof![
        Just(KeyAction::TogglePalette),
        Just(KeyAction::NextPane),
        Just(KeyAction::PreviousPane),
        Just(KeyAction::ToggleCompact),
        Just(KeyAction::CycleSort),
        Just(KeyAction::CycleHealthFilter),
        Just(KeyAction::OpenDiagnostics),
        Just(KeyAction::EmergencyStop),
        Just(KeyAction::RefreshView),
        Just(KeyAction::FocusSearch),
    ]
}

fn arb_palette_action() -> impl Strategy<Value = PaletteAction> {
    (
        "[a-z-]{3,12}",
        "[A-Za-z ]{3,20}",
        arb_action_category(),
        arb_operator_level(),
        any::<bool>(),
    )
        .prop_map(|(id, label, cat, min_level, destructive)| {
            let mut action = PaletteAction::new(id, label, cat).requires_role(min_level);
            if destructive {
                action = action.destructive();
            }
            action
        })
}

fn arb_pane_status_row() -> impl Strategy<Value = PaneStatusRow> {
    (
        "[0-9]{1,5}",
        "[a-z ]{3,15}",
        arb_pane_health(),
        0.0..100.0f64,
        0..100u32,
    )
        .prop_map(|(pane_id, title, health, cpu, alerts)| PaneStatusRow {
            pane_id,
            title,
            health,
            agent_id: None,
            last_activity_ms: 0,
            cpu_percent: cpu,
            memory_mb: 0.0,
            event_rate: 0.0,
            alert_count: alerts,
        })
}

// =============================================================================
// Serde roundtrips
// =============================================================================

proptest! {
    #[test]
    fn serde_roundtrip_operator_level(level in arb_operator_level()) {
        let json = serde_json::to_string(&level).unwrap();
        let back: OperatorLevel = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(level, back);
    }

    #[test]
    fn serde_roundtrip_action_category(cat in arb_action_category()) {
        let json = serde_json::to_string(&cat).unwrap();
        let back: ActionCategory = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(cat, back);
    }

    #[test]
    fn serde_roundtrip_pane_health(health in arb_pane_health()) {
        let json = serde_json::to_string(&health).unwrap();
        let back: PaneHealth = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(health, back);
    }

    #[test]
    fn serde_roundtrip_key_action(action in arb_key_action()) {
        let json = serde_json::to_string(&action).unwrap();
        let back: KeyAction = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(action, back);
    }

    #[test]
    fn serde_roundtrip_palette_action(action in arb_palette_action()) {
        let json = serde_json::to_string(&action).unwrap();
        let back: PaletteAction = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(action.action_id, back.action_id);
        prop_assert_eq!(action.category, back.category);
        prop_assert_eq!(action.destructive, back.destructive);
    }
}

// =============================================================================
// Operator level ordering
// =============================================================================

proptest! {
    #[test]
    fn operator_level_reflexive(level in arb_operator_level()) {
        prop_assert!(level.has_at_least(level),
            "{:?} should have at least its own level", level);
    }

    #[test]
    fn operator_level_admin_has_all(level in arb_operator_level()) {
        prop_assert!(OperatorLevel::Admin.has_at_least(level),
            "Admin should have at least {:?}", level);
    }

    #[test]
    fn operator_level_ordering_consistent(
        a in arb_operator_level(),
        b in arb_operator_level(),
    ) {
        if a.level() >= b.level() {
            prop_assert!(a.has_at_least(b));
        } else {
            prop_assert!(!a.has_at_least(b));
        }
    }

    #[test]
    fn operator_level_label_nonempty(level in arb_operator_level()) {
        prop_assert!(!level.label().is_empty());
    }
}

#[test]
fn observer_is_lowest() {
    assert!(!OperatorLevel::Observer.has_at_least(OperatorLevel::Operator));
    assert!(!OperatorLevel::Observer.has_at_least(OperatorLevel::SeniorOperator));
    assert!(!OperatorLevel::Observer.has_at_least(OperatorLevel::Admin));
}

// =============================================================================
// Action availability logic
// =============================================================================

proptest! {
    #[test]
    fn action_available_when_operator_level_sufficient(
        action in arb_palette_action(),
        level in arb_operator_level(),
    ) {
        let avail = action.availability(level, false);
        if !level.has_at_least(action.min_level) {
            prop_assert_eq!(avail, ActionAvailability::InsufficientPrivilege);
        }
    }

    #[test]
    fn destructive_action_requires_confirmation(
        cat in arb_action_category(),
    ) {
        let action = PaletteAction::new("test", "Test", cat)
            .destructive()
            .requires_role(OperatorLevel::Observer);
        let avail = action.availability(OperatorLevel::Admin, false);
        prop_assert_eq!(avail, ActionAvailability::RequiresConfirmation,
            "destructive action should require confirmation");
    }

    #[test]
    fn availability_is_executable_partitions(
        action in arb_palette_action(),
        level in arb_operator_level(),
    ) {
        let avail = action.availability(level, false);
        let executable = avail.is_executable();
        match avail {
            ActionAvailability::Available | ActionAvailability::RequiresConfirmation => {
                prop_assert!(executable);
            }
            ActionAvailability::PolicyBlocked
            | ActionAvailability::InsufficientPrivilege
            | ActionAvailability::NotApplicable => {
                prop_assert!(!executable);
            }
        }
    }
}

// =============================================================================
// Fuzzy match scoring
// =============================================================================

proptest! {
    #[test]
    fn match_score_zero_for_unrelated(
        action_label in "[xyz]{5,10}",
        cat in arb_action_category(),
    ) {
        let action = PaletteAction::new("test", action_label, cat);
        let score = action.match_score("qqqqq");
        // Unrelated query should score 0
        prop_assert_eq!(score, 0);
    }

    #[test]
    fn match_score_positive_for_exact_prefix(
        cat in arb_action_category(),
    ) {
        let action = PaletteAction::new("fleet-stop", "Fleet Stop All", cat);
        let score = action.match_score("fleet");
        prop_assert!(score > 0, "exact prefix should score > 0, got {}", score);
    }

    #[test]
    fn match_score_deterministic(
        action in arb_palette_action(),
        query in "[a-z]{1,5}",
    ) {
        let s1 = action.match_score(&query);
        let s2 = action.match_score(&query);
        prop_assert_eq!(s1, s2, "match_score should be deterministic");
    }
}

// =============================================================================
// CommandPalette invariants
// =============================================================================

proptest! {
    #[test]
    fn palette_count_matches_registrations(
        actions in prop::collection::vec(arb_palette_action(), 0..10)
    ) {
        let mut palette = CommandPalette::new();
        for a in &actions {
            palette.register(a.clone());
        }
        prop_assert_eq!(palette.action_count(), actions.len());
    }

    #[test]
    fn palette_search_returns_subset(
        actions in prop::collection::vec(arb_palette_action(), 1..10),
        query in "[a-z]{1,5}",
    ) {
        let mut palette = CommandPalette::new();
        for a in &actions {
            palette.register(a.clone());
        }
        let results = palette.search(&query);
        prop_assert!(results.len() <= palette.action_count());
    }

    #[test]
    fn palette_by_category_subset(
        actions in prop::collection::vec(arb_palette_action(), 1..10),
        cat in arb_action_category(),
    ) {
        let mut palette = CommandPalette::new();
        for a in &actions {
            palette.register(a.clone());
        }
        let filtered = palette.by_category(cat);
        prop_assert!(filtered.len() <= palette.action_count());
        for a in &filtered {
            prop_assert_eq!(a.category, cat);
        }
    }

    #[test]
    fn palette_available_actions_subset(
        actions in prop::collection::vec(arb_palette_action(), 1..10),
        level in arb_operator_level(),
    ) {
        let mut palette = CommandPalette::new();
        for a in &actions {
            palette.register(a.clone());
        }
        let available = palette.available_actions(level);
        prop_assert!(available.len() <= palette.action_count());
    }

    #[test]
    fn palette_open_close_toggles(
        actions in prop::collection::vec(arb_palette_action(), 0..5)
    ) {
        let mut palette = CommandPalette::new();
        for a in &actions {
            palette.register(a.clone());
        }
        prop_assert!(!palette.is_open);
        palette.open();
        prop_assert!(palette.is_open);
        palette.close();
        prop_assert!(!palette.is_open);
    }
}

// =============================================================================
// LiveView health summary
// =============================================================================

proptest! {
    #[test]
    fn health_summary_sums_to_total(
        panes in prop::collection::vec(arb_pane_status_row(), 0..20)
    ) {
        let mut view = LiveView::new();
        view.update_panes(panes.clone(), 1000);

        let summary = view.health_summary();
        let total: usize = summary.values().sum();
        prop_assert_eq!(total, panes.len(),
            "health summary total ({}) should equal pane count ({})", total, panes.len());
    }
}

// =============================================================================
// Latency budget checks
// =============================================================================

proptest! {
    #[test]
    fn latency_budget_within_means_under_threshold(
        measured in 0..500u64,
    ) {
        let budget = LatencyBudget::strict();
        let check = budget.check("palette_open", measured);
        if measured <= budget.palette_open_ms {
            prop_assert!(check.within_budget);
        } else {
            prop_assert!(!check.within_budget);
        }
    }

    #[test]
    fn update_throttle_respects_interval(
        interval in 10..1000u64,
        now in 0..10000u64,
    ) {
        let mut throttle = UpdateThrottle::new(interval);
        // First update should always go through
        let first = throttle.should_update(now);
        prop_assert!(first, "first update should always proceed");

        // Immediate retry should be throttled
        let retry = throttle.should_update(now);
        prop_assert!(!retry, "immediate retry should be throttled");

        // After interval, should proceed
        let later = throttle.should_update(now + interval + 1);
        prop_assert!(later, "update after interval should proceed");
    }
}

// =============================================================================
// Standard factories
// =============================================================================

#[test]
fn standard_keybindings_nonempty() {
    let bindings = standard_keybindings();
    assert!(!bindings.is_empty());
    for binding in &bindings {
        assert!(!binding.keys.is_empty());
        assert!(!binding.label.is_empty());
    }
}

#[test]
fn register_standard_actions_populates_palette() {
    let mut palette = CommandPalette::new();
    register_standard_actions(&mut palette);
    assert!(palette.action_count() > 0);
}

#[test]
fn action_category_all_has_8() {
    assert_eq!(ActionCategory::ALL.len(), 8);
}

#[test]
fn command_center_snapshot_serializes() {
    let center = SwarmCommandCenter::new(LatencyBudget::strict());
    let snap = center.snapshot();
    let json = serde_json::to_string(&snap).unwrap();
    let back: CommandCenterSnapshot = serde_json::from_str(&json).unwrap();
    assert_eq!(back.operator_level, OperatorLevel::Observer);
}
