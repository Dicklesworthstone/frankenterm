//! Property-based tests for the TUI state reducer module.
//!
//! Covers ListState navigation, pure reducer determinism, filter cycling,
//! timeline zoom/scroll bounds, DataRefreshed clamping, and search lifecycle.

#![cfg(any(feature = "tui", feature = "ftui"))]

use frankenterm_core::tui::state::{Effect, ListState, UiAction, UiState, View, reduce};
use proptest::prelude::*;

// =============================================================================
// Strategies
// =============================================================================

fn arb_view() -> impl Strategy<Value = View> {
    prop_oneof![
        Just(View::Home),
        Just(View::Panes),
        Just(View::Events),
        Just(View::Triage),
        Just(View::History),
        Just(View::Search),
        Just(View::Help),
        Just(View::Timeline),
    ]
}

fn arb_count() -> impl Strategy<Value = usize> {
    0..200usize
}

fn arb_printable_char() -> impl Strategy<Value = char> {
    prop_oneof![
        (b'a'..=b'z').prop_map(|b| b as char),
        (b'A'..=b'Z').prop_map(|b| b as char),
        (b'0'..=b'9').prop_map(|b| b as char),
        Just('_'),
        Just('-'),
        Just('.'),
    ]
}

// =============================================================================
// ListState invariant tests
// =============================================================================

proptest! {
    /// clamp(0) always sets selected to 0.
    #[test]
    fn list_state_clamp_empty_is_zero(selected in 0..1000usize) {
        let mut ls = ListState { selected };
        ls.clamp(0);
        prop_assert_eq!(ls.selected, 0);
    }

    /// clamp(count) always produces selected < count (or 0 if count == 0).
    #[test]
    fn list_state_clamp_bounded(selected in 0..1000usize, count in 0..100usize) {
        let mut ls = ListState { selected };
        ls.clamp(count);
        if count == 0 {
            prop_assert_eq!(ls.selected, 0);
        } else {
            prop_assert!(ls.selected < count);
        }
    }

    /// clamp is idempotent: clamping twice gives same result as once.
    #[test]
    fn list_state_clamp_idempotent(selected in 0..1000usize, count in 0..100usize) {
        let mut ls1 = ListState { selected };
        ls1.clamp(count);
        let after_first = ls1.selected;
        ls1.clamp(count);
        prop_assert_eq!(ls1.selected, after_first);
    }

    /// clamp preserves value when already in bounds.
    #[test]
    fn list_state_clamp_preserves_in_bounds(selected in 0..50usize, extra in 1..50usize) {
        let count = selected + extra;
        let mut ls = ListState { selected };
        ls.clamp(count);
        prop_assert_eq!(ls.selected, selected);
    }

    /// select_next wraps: after `count` calls, returns to start.
    #[test]
    fn list_state_select_next_full_cycle(start in 0..20usize, count in 1..20usize) {
        let start_clamped = if count > 0 { start % count } else { 0 };
        let mut ls = ListState { selected: start_clamped };
        for _ in 0..count {
            ls.select_next(count);
        }
        prop_assert_eq!(ls.selected, start_clamped);
    }

    /// select_prev wraps: after `count` calls, returns to start.
    #[test]
    fn list_state_select_prev_full_cycle(start in 0..20usize, count in 1..20usize) {
        let start_clamped = if count > 0 { start % count } else { 0 };
        let mut ls = ListState { selected: start_clamped };
        for _ in 0..count {
            ls.select_prev(count);
        }
        prop_assert_eq!(ls.selected, start_clamped);
    }

    /// select_next then select_prev is identity (for count > 0).
    #[test]
    fn list_state_next_prev_roundtrip(start in 0..50usize, count in 1..50usize) {
        let start_clamped = start % count;
        let mut ls = ListState { selected: start_clamped };
        ls.select_next(count);
        ls.select_prev(count);
        prop_assert_eq!(ls.selected, start_clamped);
    }

    /// select_next always produces selected < count (for count > 0).
    #[test]
    fn list_state_select_next_bounded(start in 0..50usize, count in 1..50usize) {
        let mut ls = ListState { selected: start % count };
        ls.select_next(count);
        prop_assert!(ls.selected < count);
    }

    /// select_prev always produces selected < count (for count > 0).
    #[test]
    fn list_state_select_prev_bounded(start in 0..50usize, count in 1..50usize) {
        let mut ls = ListState { selected: start % count };
        ls.select_prev(count);
        prop_assert!(ls.selected < count);
    }

    /// select_next with count=0 is a no-op.
    #[test]
    fn list_state_select_next_empty_noop(selected in 0..100usize) {
        let mut ls = ListState { selected };
        ls.select_next(0);
        prop_assert_eq!(ls.selected, selected);
    }

    /// select_prev with count=0 is a no-op.
    #[test]
    fn list_state_select_prev_empty_noop(selected in 0..100usize) {
        let mut ls = ListState { selected };
        ls.select_prev(0);
        prop_assert_eq!(ls.selected, selected);
    }
}

// =============================================================================
// View navigation via reducer
// =============================================================================

proptest! {
    /// NextView then PrevView is identity.
    #[test]
    fn reduce_next_prev_view_roundtrip(view in arb_view()) {
        let mut state = UiState { active_view: view, ..Default::default() };
        reduce(&mut state, UiAction::NextView);
        reduce(&mut state, UiAction::PrevView);
        prop_assert_eq!(state.active_view, view);
    }

    /// SwitchView sets the view exactly.
    #[test]
    fn reduce_switch_view_exact(target in arb_view()) {
        let mut state = UiState::default();
        reduce(&mut state, UiAction::SwitchView(target));
        prop_assert_eq!(state.active_view, target);
    }

    /// SwitchView is idempotent.
    #[test]
    fn reduce_switch_view_idempotent(target in arb_view()) {
        let mut state = UiState::default();
        reduce(&mut state, UiAction::SwitchView(target));
        reduce(&mut state, UiAction::SwitchView(target));
        prop_assert_eq!(state.active_view, target);
    }
}

// =============================================================================
// Reducer determinism
// =============================================================================

proptest! {
    /// Quit always sets should_quit and emits Effect::Quit.
    #[test]
    fn reduce_quit_deterministic(view in arb_view()) {
        let mut state = UiState { active_view: view, ..Default::default() };
        let effects = reduce(&mut state, UiAction::Quit);
        prop_assert!(state.should_quit);
        prop_assert_eq!(effects, vec![Effect::Quit]);
    }

    /// SelectNext in Home/Help is a no-op.
    #[test]
    fn reduce_select_next_noop_for_static_views(
        home_or_help in prop_oneof![Just(View::Home), Just(View::Help)],
    ) {
        let mut state = UiState { active_view: home_or_help, ..Default::default() };
        let effects = reduce(&mut state, UiAction::SelectNext);
        prop_assert!(effects.is_empty());
    }

    /// SelectPrev in Home/Help is a no-op.
    #[test]
    fn reduce_select_prev_noop_for_static_views(
        home_or_help in prop_oneof![Just(View::Home), Just(View::Help)],
    ) {
        let mut state = UiState { active_view: home_or_help, ..Default::default() };
        let effects = reduce(&mut state, UiAction::SelectPrev);
        prop_assert!(effects.is_empty());
    }
}

// =============================================================================
// Filter accumulation tests
// =============================================================================

proptest! {
    /// PushPanesFilterChar appends char and resets selection.
    #[test]
    fn reduce_push_panes_filter_resets_selection(
        ch in arb_printable_char(),
        initial_selected in 0..50usize,
    ) {
        let mut state = UiState {
            active_view: View::Panes,
            panes: ListState { selected: initial_selected },
            ..Default::default()
        };
        let effects = reduce(&mut state, UiAction::PushPanesFilterChar(ch));
        prop_assert_eq!(state.panes.selected, 0);
        prop_assert_eq!(state.panes_filter.chars().last(), Some(ch));
        prop_assert_eq!(effects, vec![Effect::RefreshData]);
    }

    /// Pushing N chars then popping N chars empties the filter.
    #[test]
    fn reduce_panes_filter_push_pop_symmetric(
        chars in prop::collection::vec(arb_printable_char(), 1..20),
    ) {
        let mut state = UiState::default();
        let n = chars.len();
        for ch in &chars {
            reduce(&mut state, UiAction::PushPanesFilterChar(*ch));
        }
        prop_assert_eq!(state.panes_filter.len(), n);
        for _ in 0..n {
            reduce(&mut state, UiAction::PopPanesFilterChar);
        }
        prop_assert!(state.panes_filter.is_empty());
    }

    /// PushHistoryFilterChar resets history selection.
    #[test]
    fn reduce_push_history_filter_resets_selection(
        ch in arb_printable_char(),
        initial_selected in 0..50usize,
    ) {
        let mut state = UiState {
            active_view: View::History,
            history: ListState { selected: initial_selected },
            ..Default::default()
        };
        reduce(&mut state, UiAction::PushHistoryFilterChar(ch));
        prop_assert_eq!(state.history.selected, 0);
        prop_assert_eq!(state.history_filter.chars().last(), Some(ch));
    }

    /// ClearPanesFilters resets all pane-related filter state.
    #[test]
    fn reduce_clear_panes_resets_all(
        filter in "[a-z]{1,10}",
        selected in 0..50usize,
    ) {
        let mut state = UiState {
            panes_filter: filter,
            panes_unhandled_only: true,
            panes_bookmarked_only: true,
            panes_agent_filter: Some("codex".to_string()),
            panes_domain_filter: Some("local".to_string()),
            panes: ListState { selected },
            ..Default::default()
        };
        reduce(&mut state, UiAction::ClearPanesFilters);
        prop_assert!(state.panes_filter.is_empty());
        prop_assert!(!state.panes_unhandled_only);
        prop_assert!(!state.panes_bookmarked_only);
        prop_assert!(state.panes_agent_filter.is_none());
        prop_assert!(state.panes_domain_filter.is_none());
        prop_assert_eq!(state.panes.selected, 0);
    }
}

// =============================================================================
// Toggle invariants
// =============================================================================

proptest! {
    /// ToggleUnhandledOnly is self-inverse (double-toggle restores original).
    #[test]
    fn reduce_toggle_unhandled_self_inverse(initial in any::<bool>()) {
        let mut state = UiState {
            panes_unhandled_only: initial,
            ..Default::default()
        };
        reduce(&mut state, UiAction::ToggleUnhandledOnly);
        reduce(&mut state, UiAction::ToggleUnhandledOnly);
        prop_assert_eq!(state.panes_unhandled_only, initial);
    }

    /// ToggleBookmarkedOnly is self-inverse.
    #[test]
    fn reduce_toggle_bookmarked_self_inverse(initial in any::<bool>()) {
        let mut state = UiState {
            panes_bookmarked_only: initial,
            ..Default::default()
        };
        reduce(&mut state, UiAction::ToggleBookmarkedOnly);
        reduce(&mut state, UiAction::ToggleBookmarkedOnly);
        prop_assert_eq!(state.panes_bookmarked_only, initial);
    }

    /// ToggleEventsUnhandled is self-inverse.
    #[test]
    fn reduce_toggle_events_unhandled_self_inverse(initial in any::<bool>()) {
        let mut state = UiState {
            events_unhandled_only: initial,
            ..Default::default()
        };
        reduce(&mut state, UiAction::ToggleEventsUnhandled);
        reduce(&mut state, UiAction::ToggleEventsUnhandled);
        prop_assert_eq!(state.events_unhandled_only, initial);
    }

    /// ToggleHistoryUndoable is self-inverse.
    #[test]
    fn reduce_toggle_history_undoable_self_inverse(initial in any::<bool>()) {
        let mut state = UiState {
            history_undoable_only: initial,
            ..Default::default()
        };
        reduce(&mut state, UiAction::ToggleHistoryUndoable);
        reduce(&mut state, UiAction::ToggleHistoryUndoable);
        prop_assert_eq!(state.history_undoable_only, initial);
    }
}

// =============================================================================
// Timeline zoom/scroll bounds
// =============================================================================

proptest! {
    /// Timeline zoom never exceeds 5.
    #[test]
    fn reduce_timeline_zoom_bounded(zoom_steps in 0..20usize) {
        let mut state = UiState::default();
        for _ in 0..zoom_steps {
            reduce(&mut state, UiAction::TimelineZoomIn);
        }
        prop_assert!(state.timeline_zoom <= 5);
    }

    /// Timeline zoom out never goes below 0 (saturating).
    #[test]
    fn reduce_timeline_zoom_out_saturates(
        initial_zoom in 0..6u8,
        zoom_out_steps in 0..20usize,
    ) {
        let mut state = UiState {
            timeline_zoom: initial_zoom,
            ..Default::default()
        };
        for _ in 0..zoom_out_steps {
            reduce(&mut state, UiAction::TimelineZoomOut);
        }
        // timeline_zoom is u8, saturating_sub prevents underflow
        prop_assert!(state.timeline_zoom <= initial_zoom);
    }

    /// Timeline scroll left never goes below 0 (saturating).
    #[test]
    fn reduce_timeline_scroll_left_saturates(
        initial_scroll in 0..100usize,
        scroll_steps in 0..200usize,
    ) {
        let mut state = UiState {
            timeline_scroll: initial_scroll,
            ..Default::default()
        };
        for _ in 0..scroll_steps {
            reduce(&mut state, UiAction::TimelineScrollLeft);
        }
        // Must not underflow
        prop_assert!(state.timeline_scroll <= initial_scroll);
    }

    /// Timeline scroll right is bounded by timeline_count.
    #[test]
    fn reduce_timeline_scroll_right_bounded(
        count in 1..100usize,
        scroll_steps in 0..200usize,
    ) {
        let mut state = UiState {
            timeline_count: count,
            ..Default::default()
        };
        for _ in 0..scroll_steps {
            reduce(&mut state, UiAction::TimelineScrollRight);
        }
        prop_assert!(state.timeline_scroll < count);
    }

    /// Timeline scroll right with count=0 is a no-op.
    #[test]
    fn reduce_timeline_scroll_right_empty_noop(scroll_steps in 1..20usize) {
        let mut state = UiState {
            timeline_count: 0,
            ..Default::default()
        };
        for _ in 0..scroll_steps {
            reduce(&mut state, UiAction::TimelineScrollRight);
        }
        prop_assert_eq!(state.timeline_scroll, 0);
    }
}

// =============================================================================
// DataRefreshed clamping
// =============================================================================

proptest! {
    /// After DataRefreshed, all list selections are within bounds.
    #[test]
    fn reduce_data_refreshed_clamps_all_selections(
        panes_sel in 0..100usize,
        events_sel in 0..100usize,
        triage_sel in 0..100usize,
        history_sel in 0..100usize,
        timeline_sel in 0..100usize,
        pfc in arb_count(),
        efc in arb_count(),
        tc in arb_count(),
        hfc in arb_count(),
        tlc in arb_count(),
    ) {
        let mut state = UiState {
            panes: ListState { selected: panes_sel },
            events: ListState { selected: events_sel },
            triage: ListState { selected: triage_sel },
            history: ListState { selected: history_sel },
            timeline: ListState { selected: timeline_sel },
            ..Default::default()
        };
        reduce(&mut state, UiAction::DataRefreshed {
            panes_count: pfc + 10, // panes_count doesn't clamp directly
            panes_filtered_count: pfc,
            events_count: efc + 10,
            events_filtered_count: efc,
            triage_count: tc,
            history_count: hfc + 10,
            history_filtered_count: hfc,
            saved_searches_count: 5,
            profiles_count: 3,
            timeline_count: tlc,
        });

        if pfc == 0 { prop_assert_eq!(state.panes.selected, 0); }
        else { prop_assert!(state.panes.selected < pfc); }

        if efc == 0 { prop_assert_eq!(state.events.selected, 0); }
        else { prop_assert!(state.events.selected < efc); }

        if tc == 0 { prop_assert_eq!(state.triage.selected, 0); }
        else { prop_assert!(state.triage.selected < tc); }

        if hfc == 0 { prop_assert_eq!(state.history.selected, 0); }
        else { prop_assert!(state.history.selected < hfc); }

        if tlc == 0 { prop_assert_eq!(state.timeline.selected, 0); }
        else { prop_assert!(state.timeline.selected < tlc); }
    }

    /// DataRefreshed clears error.
    #[test]
    fn reduce_data_refreshed_clears_error(msg in "[a-z]{1,20}") {
        let mut state = UiState {
            error: Some(msg),
            ..Default::default()
        };
        reduce(&mut state, UiAction::DataRefreshed {
            panes_count: 0,
            panes_filtered_count: 0,
            events_count: 0,
            events_filtered_count: 0,
            triage_count: 0,
            history_count: 0,
            history_filtered_count: 0,
            saved_searches_count: 0,
            profiles_count: 0,
            timeline_count: 0,
        });
        prop_assert!(state.error.is_none());
    }

    /// DataRefreshed invalidates triage_expanded when out of range.
    #[test]
    fn reduce_data_refreshed_invalidates_triage_expanded(
        expanded_idx in 0..100usize,
        triage_count in 0..100usize,
    ) {
        let mut state = UiState {
            triage_expanded: Some(expanded_idx),
            ..Default::default()
        };
        reduce(&mut state, UiAction::DataRefreshed {
            panes_count: 0,
            panes_filtered_count: 0,
            events_count: 0,
            events_filtered_count: 0,
            triage_count,
            history_count: 0,
            history_filtered_count: 0,
            saved_searches_count: 0,
            profiles_count: 0,
            timeline_count: 0,
        });
        if expanded_idx >= triage_count {
            prop_assert!(state.triage_expanded.is_none());
        } else {
            prop_assert_eq!(state.triage_expanded, Some(expanded_idx));
        }
    }
}

// =============================================================================
// Search lifecycle
// =============================================================================

proptest! {
    /// SubmitSearch on empty query emits no effects.
    #[test]
    fn reduce_submit_empty_search_noop(_dummy in 0..10u8) {
        let mut state = UiState::default();
        let effects = reduce(&mut state, UiAction::SubmitSearch);
        prop_assert!(effects.is_empty());
    }

    /// SubmitSearch emits ExecuteSearch with the current query.
    #[test]
    fn reduce_submit_search_emits_execute(query in "[a-z]{1,20}") {
        let mut state = UiState {
            search_query: query.clone(),
            ..Default::default()
        };
        let effects = reduce(&mut state, UiAction::SubmitSearch);
        prop_assert_eq!(effects, vec![Effect::ExecuteSearch(query.clone())]);
        prop_assert_eq!(state.search_last_query, query);
    }

    /// SearchCompleted for stale query does not update results.
    #[test]
    fn reduce_stale_search_ignored(
        current_query in "[a-z]{1,10}",
        stale_query in "[A-Z]{1,10}",
        result_count in 0..100usize,
    ) {
        let mut state = UiState {
            search_last_query: current_query,
            ..Default::default()
        };
        reduce(&mut state, UiAction::SearchCompleted {
            query: stale_query,
            result_count,
        });
        prop_assert_eq!(state.search_results_count, 0); // unchanged from default
    }

    /// SearchCompleted for matching query updates count.
    #[test]
    fn reduce_matching_search_updates(
        query in "[a-z]{1,10}",
        result_count in 0..100usize,
    ) {
        let mut state = UiState {
            search_last_query: query.clone(),
            ..Default::default()
        };
        reduce(&mut state, UiAction::SearchCompleted {
            query: query.clone(),
            result_count,
        });
        prop_assert_eq!(state.search_results_count, result_count);
    }

    /// ClearSearch empties all search state.
    #[test]
    fn reduce_clear_search_resets(
        query in "[a-z]{1,10}",
        last_query in "[a-z]{1,10}",
        result_count in 1..100usize,
    ) {
        let mut state = UiState {
            search_query: query,
            search_last_query: last_query,
            search_results_count: result_count,
            search_results: ListState { selected: 5 },
            ..Default::default()
        };
        reduce(&mut state, UiAction::ClearSearch);
        prop_assert!(state.search_query.is_empty());
        prop_assert!(state.search_last_query.is_empty());
        prop_assert_eq!(state.search_results_count, 0);
        prop_assert_eq!(state.search_results.selected, 0);
    }
}

// =============================================================================
// CycleProfile and CycleSavedSearch
// =============================================================================

proptest! {
    /// CycleProfile wraps at profiles_count.
    #[test]
    fn reduce_cycle_profile_wraps(
        profiles_count in 1..20usize,
        steps in 0..50usize,
    ) {
        let mut state = UiState {
            profiles_count,
            ..Default::default()
        };
        for _ in 0..steps {
            reduce(&mut state, UiAction::CycleProfile);
        }
        prop_assert!(state.panes_profile_index < profiles_count);
    }

    /// CycleProfile with 0 profiles is a no-op.
    #[test]
    fn reduce_cycle_profile_zero_noop(steps in 1..20usize) {
        let mut state = UiState {
            profiles_count: 0,
            ..Default::default()
        };
        for _ in 0..steps {
            reduce(&mut state, UiAction::CycleProfile);
        }
        prop_assert_eq!(state.panes_profile_index, 0);
    }

    /// CycleSavedSearchNext wraps at saved_searches_count.
    #[test]
    fn reduce_cycle_saved_search_next_wraps(
        saved_count in 1..20usize,
        steps in 0..50usize,
    ) {
        let mut state = UiState {
            saved_searches_count: saved_count,
            ..Default::default()
        };
        for _ in 0..steps {
            reduce(&mut state, UiAction::CycleSavedSearchNext);
        }
        prop_assert!(state.saved_search_index < saved_count);
    }

    /// CycleSavedSearchPrev wraps at saved_searches_count.
    #[test]
    fn reduce_cycle_saved_search_prev_wraps(
        saved_count in 1..20usize,
        steps in 0..50usize,
    ) {
        let mut state = UiState {
            saved_searches_count: saved_count,
            ..Default::default()
        };
        for _ in 0..steps {
            reduce(&mut state, UiAction::CycleSavedSearchPrev);
        }
        prop_assert!(state.saved_search_index < saved_count);
    }

    /// N rounds of CycleSavedSearchNext returns to start.
    #[test]
    fn reduce_cycle_saved_search_next_full_cycle(saved_count in 1..20usize) {
        let mut state = UiState {
            saved_searches_count: saved_count,
            ..Default::default()
        };
        for _ in 0..saved_count {
            reduce(&mut state, UiAction::CycleSavedSearchNext);
        }
        prop_assert_eq!(state.saved_search_index, 0);
    }

    /// CycleSavedSearchNext then CycleSavedSearchPrev is identity.
    #[test]
    fn reduce_cycle_saved_search_roundtrip(saved_count in 1..20usize) {
        let mut state = UiState {
            saved_searches_count: saved_count,
            ..Default::default()
        };
        reduce(&mut state, UiAction::CycleSavedSearchNext);
        reduce(&mut state, UiAction::CycleSavedSearchPrev);
        prop_assert_eq!(state.saved_search_index, 0);
    }
}

// =============================================================================
// SelectNext/SelectPrev dispatch to correct view
// =============================================================================

proptest! {
    /// SelectNext in Panes only modifies panes.selected.
    #[test]
    fn reduce_select_next_panes_isolation(count in 1..50usize) {
        let mut state = UiState {
            active_view: View::Panes,
            panes_filtered_count: count,
            events_filtered_count: count,
            ..Default::default()
        };
        reduce(&mut state, UiAction::SelectNext);
        prop_assert_eq!(state.panes.selected, 1 % count);
        prop_assert_eq!(state.events.selected, 0); // untouched
    }

    /// SelectNext in Events only modifies events.selected.
    #[test]
    fn reduce_select_next_events_isolation(count in 1..50usize) {
        let mut state = UiState {
            active_view: View::Events,
            panes_filtered_count: count,
            events_filtered_count: count,
            ..Default::default()
        };
        reduce(&mut state, UiAction::SelectNext);
        prop_assert_eq!(state.events.selected, 1 % count);
        prop_assert_eq!(state.panes.selected, 0); // untouched
    }

    /// SelectNext wraps correctly for each view with items.
    #[test]
    fn reduce_select_next_wraps_per_view(
        view in prop_oneof![
            Just(View::Panes),
            Just(View::Events),
            Just(View::Triage),
            Just(View::History),
            Just(View::Search),
            Just(View::Timeline),
        ],
        count in 1..20usize,
    ) {
        let mut state = UiState {
            active_view: view,
            panes_filtered_count: count,
            events_filtered_count: count,
            triage_count: count,
            history_filtered_count: count,
            search_results_count: count,
            timeline_count: count,
            ..Default::default()
        };

        // Start at last item
        let list = match view {
            View::Panes => &mut state.panes,
            View::Events => &mut state.events,
            View::Triage => &mut state.triage,
            View::History => &mut state.history,
            View::Search => &mut state.search_results,
            View::Timeline => &mut state.timeline,
            _ => unreachable!(),
        };
        list.selected = count - 1;

        reduce(&mut state, UiAction::SelectNext);

        let selected = match view {
            View::Panes => state.panes.selected,
            View::Events => state.events.selected,
            View::Triage => state.triage.selected,
            View::History => state.history.selected,
            View::Search => state.search_results.selected,
            View::Timeline => state.timeline.selected,
            _ => unreachable!(),
        };
        prop_assert_eq!(selected, 0); // Wrapped
    }
}

// =============================================================================
// Error lifecycle
// =============================================================================

proptest! {
    /// DataError sets error, ClearError removes it.
    #[test]
    fn reduce_error_lifecycle(msg in "[a-z ]{1,50}") {
        let mut state = UiState::default();
        reduce(&mut state, UiAction::DataError(msg.clone()));
        prop_assert_eq!(state.error.as_deref(), Some(msg.as_str()));
        reduce(&mut state, UiAction::ClearError);
        prop_assert!(state.error.is_none());
    }

    /// QueueCommand emits RunCommand effect.
    #[test]
    fn reduce_queue_command_emits_effect(cmd in "[a-z]{1,20}") {
        let mut state = UiState::default();
        let effects = reduce(&mut state, UiAction::QueueCommand(cmd.clone()));
        prop_assert_eq!(effects, vec![Effect::RunCommand(cmd)]);
    }
}

// =============================================================================
// Triage expand/collapse
// =============================================================================

proptest! {
    /// ToggleTriageExpanded twice restores original state.
    #[test]
    fn reduce_triage_expand_self_inverse(selected in 0..20usize) {
        let mut state = UiState {
            triage: ListState { selected },
            ..Default::default()
        };
        let original = state.triage_expanded;
        reduce(&mut state, UiAction::ToggleTriageExpanded);
        reduce(&mut state, UiAction::ToggleTriageExpanded);
        prop_assert_eq!(state.triage_expanded, original);
    }

    /// ToggleTriageExpanded sets expanded to current selection.
    #[test]
    fn reduce_triage_expand_sets_selected(selected in 0..20usize) {
        let mut state = UiState {
            triage: ListState { selected },
            triage_expanded: None,
            ..Default::default()
        };
        reduce(&mut state, UiAction::ToggleTriageExpanded);
        prop_assert_eq!(state.triage_expanded, Some(selected));
    }
}
