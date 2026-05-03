#![cfg(any(feature = "tui", feature = "ftui"))]

use std::collections::HashMap;

use frankenterm_core::storage::Timeline;
use frankenterm_core::tui::{
    EventFilters, EventView, HealthStatus, HistoryEntryView, PaneView, QueryClient, QueryError,
    SearchResultView, TriageAction, TriageItemView, WorkflowProgressView,
};
use frankenterm_core::wezterm::{CursorVisibility, PaneInfo};
use proptest::prelude::*;

struct EmptyQueryClient;

impl QueryClient for EmptyQueryClient {
    fn list_panes(&self) -> Result<Vec<PaneView>, QueryError> {
        Ok(Vec::new())
    }

    fn list_events(&self, _filters: &EventFilters) -> Result<Vec<EventView>, QueryError> {
        Ok(Vec::new())
    }

    fn list_triage_items(&self) -> Result<Vec<TriageItemView>, QueryError> {
        Ok(Vec::new())
    }

    fn search(&self, _query: &str, _limit: usize) -> Result<Vec<SearchResultView>, QueryError> {
        Ok(Vec::new())
    }

    fn health(&self) -> Result<HealthStatus, QueryError> {
        Ok(HealthStatus {
            watcher_running: false,
            db_accessible: false,
            wezterm_accessible: false,
            wezterm_circuit: Default::default(),
            pane_count: 0,
            event_count: 0,
            last_capture_ts: None,
        })
    }

    fn is_watcher_running(&self) -> bool {
        false
    }

    fn mark_event_muted(&self, _event_id: i64) -> Result<(), QueryError> {
        Ok(())
    }

    fn list_active_workflows(&self) -> Result<Vec<WorkflowProgressView>, QueryError> {
        Ok(Vec::new())
    }
}

fn bounded_text() -> impl Strategy<Value = String> {
    "[A-Za-z0-9 _./:-]{0,48}"
}

fn optional_text() -> impl Strategy<Value = Option<String>> {
    prop::option::of(bounded_text())
}

fn pane_info(
    pane_id: u64,
    title: Option<String>,
    cwd: Option<String>,
    domain_name: Option<String>,
    cursor_visibility: Option<CursorVisibility>,
    is_active: bool,
    alt_screen: Option<bool>,
) -> PaneInfo {
    let mut extra = HashMap::new();
    if let Some(value) = alt_screen {
        extra.insert(
            "is_alt_screen_active".to_string(),
            serde_json::Value::Bool(value),
        );
    }

    PaneInfo {
        pane_id,
        tab_id: pane_id.saturating_add(1),
        window_id: pane_id.saturating_add(2),
        domain_id: None,
        domain_name,
        workspace: None,
        size: None,
        rows: None,
        cols: None,
        title,
        cwd,
        tty_name: None,
        cursor_x: None,
        cursor_y: None,
        cursor_visibility,
        left_col: None,
        top_row: None,
        is_active,
        is_zoomed: false,
        extra,
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn proptest_tui_pane_view_from_pane_info_preserves_public_fields(
        pane_id in any::<u64>(),
        title in optional_text(),
        cwd in optional_text(),
        domain_name in optional_text(),
        is_active in any::<bool>(),
    ) {
        let info = pane_info(
            pane_id,
            title.clone(),
            cwd.clone(),
            domain_name.clone(),
            None,
            is_active,
            None,
        );
        let view = PaneView::from(&info);

        prop_assert_eq!(view.pane_id, pane_id);
        prop_assert_eq!(view.title, title.unwrap_or_default());
        prop_assert_eq!(view.domain, domain_name.unwrap_or_else(|| "local".to_string()));
        prop_assert_eq!(view.cwd, cwd);
        prop_assert!(!view.is_excluded);
        prop_assert_eq!(view.last_activity_ts, None);
        prop_assert_eq!(view.unhandled_event_count, 0);
    }

    #[test]
    fn proptest_tui_pane_view_agent_type_infers_from_title_or_cwd(
        prefix in prop::sample::select(vec!["codex", "claude", "gemini"]),
        tail in bounded_text(),
        use_title in any::<bool>(),
    ) {
        let marker = format!("{prefix}-{tail}");
        let info = if use_title {
            pane_info(1, Some(marker), None, None, None, false, None)
        } else {
            pane_info(1, None, Some(format!("/tmp/{marker}")), None, None, false, None)
        };
        let view = PaneView::from(&info);

        prop_assert_eq!(view.agent_type, Some(prefix.to_string()));
    }

    #[test]
    fn proptest_tui_pane_view_state_priority_is_alt_screen_cursor_active(
        hidden_cursor in any::<bool>(),
        is_active in any::<bool>(),
    ) {
        let cursor = hidden_cursor.then_some(CursorVisibility::Hidden);
        let alt = PaneView::from(&pane_info(1, None, None, None, cursor, is_active, Some(true)));
        prop_assert_eq!(alt.pane_state, "AltScreen");

        let no_alt = PaneView::from(&pane_info(1, None, None, None, cursor, is_active, Some(false)));
        let expected = if hidden_cursor {
            "CommandRunning"
        } else if is_active {
            "PromptActive"
        } else {
            "unknown"
        };
        prop_assert_eq!(no_alt.pane_state, expected);
    }

    #[test]
    fn proptest_tui_event_filters_clone_preserves_query_limits(
        pane_id in prop::option::of(any::<u64>()),
        rule_id in optional_text(),
        event_type in optional_text(),
        unhandled_only in any::<bool>(),
        limit in 0_usize..=10_000,
    ) {
        let filters = EventFilters {
            pane_id,
            rule_id,
            event_type,
            unhandled_only,
            limit,
        };
        let cloned = filters.clone();

        prop_assert_eq!(cloned.pane_id, filters.pane_id);
        prop_assert_eq!(cloned.rule_id, filters.rule_id);
        prop_assert_eq!(cloned.event_type, filters.event_type);
        prop_assert_eq!(cloned.unhandled_only, filters.unhandled_only);
        prop_assert_eq!(cloned.limit, limit);
    }

    #[test]
    fn proptest_tui_query_client_default_methods_are_empty_and_stable(
        last_ms in any::<i64>(),
        limit in 0_usize..=10_000,
    ) {
        let client = EmptyQueryClient;
        let timeline = client.get_timeline(last_ms, limit).expect("default timeline should succeed");

        let expected = Timeline {
            start: 0,
            end: 0,
            events: Vec::new(),
            correlations: Vec::new(),
            total_count: 0,
            has_more: false,
        };
        prop_assert_eq!(timeline.start, expected.start);
        prop_assert_eq!(timeline.end, expected.end);
        prop_assert!(timeline.events.is_empty());
        prop_assert!(timeline.correlations.is_empty());
        prop_assert_eq!(timeline.total_count, expected.total_count);
        prop_assert_eq!(timeline.has_more, expected.has_more);
        prop_assert!(client.list_action_history(limit).expect("default history should succeed").is_empty());
        prop_assert!(client.list_pane_bookmarks().expect("default bookmarks should succeed").is_empty());
        prop_assert!(client.list_saved_searches().expect("default saved searches should succeed").is_empty());
        prop_assert!(client.dashboard_state().expect("default dashboard should succeed").is_none());
        prop_assert!(!client.is_watcher_running());
    }

    #[test]
    fn proptest_tui_view_struct_clones_preserve_user_visible_fields(
        label in bounded_text(),
        command in bounded_text(),
        workflow_id in optional_text(),
        pane_id in prop::option::of(any::<u64>()),
        undoable in any::<bool>(),
        undone in any::<bool>(),
    ) {
        let action = TriageAction {
            label: label.clone(),
            command: command.clone(),
        };
        let triage = TriageItemView {
            section: label.clone(),
            severity: "info".to_string(),
            title: label.clone(),
            detail: command.clone(),
            actions: vec![action.clone()],
            event_id: Some(7),
            pane_id,
            workflow_id: workflow_id.clone(),
        };
        let history = HistoryEntryView {
            audit_id: 9,
            timestamp: 11,
            pane_id,
            workflow_id,
            action_kind: label.clone(),
            result: command.clone(),
            actor_kind: "robot".to_string(),
            step_name: None,
            undoable,
            undone,
            undo_strategy: None,
            undo_hint: None,
            rule_id: None,
            summary: label,
        };

        prop_assert_eq!(action.clone().command, action.command);
        let triage_clone = triage.clone();
        prop_assert_eq!(triage_clone.actions.len(), triage.actions.len());
        prop_assert_eq!(&triage_clone.actions[0].label, &triage.actions[0].label);
        prop_assert_eq!(
            &triage_clone.actions[0].command,
            &triage.actions[0].command
        );
        prop_assert_eq!(history.clone().undoable, undoable);
        prop_assert_eq!(history.clone().undone, undone);
    }
}
