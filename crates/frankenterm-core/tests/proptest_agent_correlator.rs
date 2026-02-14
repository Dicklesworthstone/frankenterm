//! Property-based tests for the `agent_correlator` module.
//!
//! Covers `DetectionSource` serde roundtrips, snake_case serialization,
//! `AgentCorrelator` lifecycle properties (new, ingest, remove, get_metadata),
//! title-based detection via `update_from_pane_info`, and state tracking.

use frankenterm_core::agent_correlator::{AgentCorrelator, DetectionSource};
use frankenterm_core::patterns::{AgentType, Detection, Severity};
use frankenterm_core::wezterm::PaneInfo;
use proptest::prelude::*;
use std::collections::HashMap;

// =========================================================================
// Strategies
// =========================================================================

fn arb_detection_source() -> impl Strategy<Value = DetectionSource> {
    prop_oneof![
        Just(DetectionSource::PatternEngine),
        Just(DetectionSource::PaneTitle),
        Just(DetectionSource::ProcessName),
    ]
}

fn arb_agent_type() -> impl Strategy<Value = AgentType> {
    prop_oneof![
        Just(AgentType::ClaudeCode),
        Just(AgentType::Codex),
        Just(AgentType::Gemini),
    ]
}

fn arb_rule_suffix() -> impl Strategy<Value = &'static str> {
    prop_oneof![
        Just("banner"),
        Just("session.start"),
        Just("tool_use"),
        Just("compaction"),
        Just("rate_limited"),
        Just("usage.reached"),
        Just("approval_needed"),
        Just("cost_summary"),
        Just("session.end"),
        Just("auth.api_key_error"),
        Just("some_other_rule"),
    ]
}

fn make_detection(rule_id: &str, agent_type: AgentType) -> Detection {
    Detection {
        rule_id: rule_id.to_string(),
        agent_type,
        event_type: "test".to_string(),
        severity: Severity::Info,
        confidence: 0.9,
        extracted: serde_json::json!({}),
        matched_text: String::new(),
        span: (0, 0),
    }
}

fn make_detection_with_session(
    rule_id: &str,
    agent_type: AgentType,
    session_id: &str,
) -> Detection {
    Detection {
        rule_id: rule_id.to_string(),
        agent_type,
        event_type: "test".to_string(),
        severity: Severity::Info,
        confidence: 0.9,
        extracted: serde_json::json!({"session_id": session_id}),
        matched_text: String::new(),
        span: (0, 0),
    }
}

fn make_pane_info(pane_id: u64, title: Option<&str>) -> PaneInfo {
    PaneInfo {
        pane_id,
        tab_id: 0,
        window_id: 0,
        domain_id: None,
        domain_name: None,
        workspace: None,
        size: None,
        rows: None,
        cols: None,
        title: title.map(String::from),
        cwd: None,
        tty_name: None,
        cursor_x: None,
        cursor_y: None,
        cursor_visibility: None,
        left_col: None,
        top_row: None,
        is_active: true,
        is_zoomed: false,
        extra: HashMap::new(),
    }
}

fn make_pane_info_with_process(pane_id: u64, process: &str) -> PaneInfo {
    let mut extra = HashMap::new();
    extra.insert(
        "foreground_process_name".to_string(),
        serde_json::Value::String(process.to_string()),
    );
    PaneInfo {
        pane_id,
        tab_id: 0,
        window_id: 0,
        domain_id: None,
        domain_name: None,
        workspace: None,
        size: None,
        rows: None,
        cols: None,
        title: None,
        cwd: None,
        tty_name: None,
        cursor_x: None,
        cursor_y: None,
        cursor_visibility: None,
        left_col: None,
        top_row: None,
        is_active: true,
        is_zoomed: false,
        extra,
    }
}

// =========================================================================
// DetectionSource — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// DetectionSource serde roundtrip.
    #[test]
    fn prop_detection_source_serde(source in arb_detection_source()) {
        let json = serde_json::to_string(&source).unwrap();
        let back: DetectionSource = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, source);
    }

    /// DetectionSource serializes to snake_case.
    #[test]
    fn prop_detection_source_snake_case(source in arb_detection_source()) {
        let json = serde_json::to_string(&source).unwrap();
        let expected = match source {
            DetectionSource::PatternEngine => "\"pattern_engine\"",
            DetectionSource::PaneTitle => "\"pane_title\"",
            DetectionSource::ProcessName => "\"process_name\"",
        };
        prop_assert_eq!(json.as_str(), expected);
    }

    /// DetectionSource serde is deterministic.
    #[test]
    fn prop_detection_source_deterministic(source in arb_detection_source()) {
        let j1 = serde_json::to_string(&source).unwrap();
        let j2 = serde_json::to_string(&source).unwrap();
        prop_assert_eq!(&j1, &j2);
    }
}

// =========================================================================
// AgentCorrelator — lifecycle properties
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// New correlator starts with zero tracked panes.
    #[test]
    fn prop_new_correlator_empty(_dummy in 0..1_u8) {
        let correlator = AgentCorrelator::new();
        prop_assert_eq!(correlator.tracked_pane_count(), 0);
    }

    /// Default correlator is the same as new().
    #[test]
    fn prop_default_is_new(_dummy in 0..1_u8) {
        let a = AgentCorrelator::new();
        let b = AgentCorrelator::default();
        prop_assert_eq!(a.tracked_pane_count(), b.tracked_pane_count());
    }

    /// get_metadata returns None for any pane ID on a fresh correlator.
    #[test]
    fn prop_fresh_correlator_no_metadata(pane_id in 0_u64..10_000) {
        let correlator = AgentCorrelator::new();
        prop_assert!(correlator.get_metadata(pane_id).is_none());
    }

    /// remove_pane on a fresh correlator is a no-op.
    #[test]
    fn prop_remove_pane_noop_on_fresh(pane_id in 0_u64..10_000) {
        let mut correlator = AgentCorrelator::new();
        correlator.remove_pane(pane_id);
        prop_assert_eq!(correlator.tracked_pane_count(), 0);
    }
}

// =========================================================================
// AgentCorrelator — ingest_detections properties
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// Ingesting a detection for a known agent type makes it trackable.
    #[test]
    fn prop_ingest_tracks_agent(
        pane_id in 0_u64..10_000,
        agent in arb_agent_type(),
        suffix in arb_rule_suffix(),
    ) {
        let mut correlator = AgentCorrelator::new();
        let rule_id = format!("core.test:{}", suffix);
        let detections = vec![make_detection(&rule_id, agent)];
        correlator.ingest_detections(pane_id, &detections);

        let meta = correlator.get_metadata(pane_id);
        prop_assert!(meta.is_some(), "should have metadata after ingest");

        let meta = meta.unwrap();
        // agent_type should be the Display form
        let expected_type = format!("{}", agent);
        prop_assert_eq!(&meta.agent_type, &expected_type);
    }

    /// Ingesting detections for N distinct pane IDs tracks N panes.
    #[test]
    fn prop_ingest_multiple_panes_counted(n in 1_u64..20) {
        let mut correlator = AgentCorrelator::new();
        for i in 0..n {
            let detections = vec![make_detection(
                "core.claude_code:banner",
                AgentType::ClaudeCode,
            )];
            correlator.ingest_detections(i, &detections);
        }
        prop_assert_eq!(correlator.tracked_pane_count(), n as usize);
    }

    /// State is inferred from rule suffix.
    #[test]
    fn prop_state_from_rule(suffix in arb_rule_suffix()) {
        let mut correlator = AgentCorrelator::new();
        let rule_id = format!("core.claude_code:{}", suffix);
        correlator.ingest_detections(1, &[make_detection(&rule_id, AgentType::ClaudeCode)]);

        let meta = correlator.get_metadata(1).unwrap();
        let state = meta.state.as_deref().unwrap();
        let expected = match suffix {
            "banner" | "session.start" => "starting",
            "tool_use" | "compaction" => "working",
            "rate_limited" | "usage.reached" => "rate_limited",
            "approval_needed" => "waiting_approval",
            "cost_summary" | "session.end" => "idle",
            "auth.api_key_error" => "auth_error",
            _ => "active",
        };
        prop_assert_eq!(state, expected, "rule suffix '{}' should map to state '{}'", suffix, expected);
    }

    /// Wezterm agent type is ignored by ingest_detections.
    #[test]
    fn prop_wezterm_ignored(pane_id in 0_u64..10_000) {
        let mut correlator = AgentCorrelator::new();
        correlator.ingest_detections(
            pane_id,
            &[make_detection("core.wezterm:event", AgentType::Wezterm)],
        );
        prop_assert_eq!(correlator.tracked_pane_count(), 0);
        prop_assert!(correlator.get_metadata(pane_id).is_none());
    }

    /// Unknown agent type is ignored by ingest_detections.
    #[test]
    fn prop_unknown_ignored(pane_id in 0_u64..10_000) {
        let mut correlator = AgentCorrelator::new();
        correlator.ingest_detections(
            pane_id,
            &[make_detection("unknown:rule", AgentType::Unknown)],
        );
        prop_assert_eq!(correlator.tracked_pane_count(), 0);
    }

    /// Ingesting multiple detections for the same pane updates state.
    #[test]
    fn prop_ingest_updates_state(pane_id in 0_u64..10_000) {
        let mut correlator = AgentCorrelator::new();

        // First: banner → starting
        correlator.ingest_detections(
            pane_id,
            &[make_detection("core.claude_code:banner", AgentType::ClaudeCode)],
        );
        let state1 = correlator.get_metadata(pane_id).unwrap().state.clone();
        prop_assert_eq!(state1.as_deref(), Some("starting"));

        // Second: tool_use → working
        correlator.ingest_detections(
            pane_id,
            &[make_detection("core.claude_code:tool_use", AgentType::ClaudeCode)],
        );
        let state2 = correlator.get_metadata(pane_id).unwrap().state.clone();
        prop_assert_eq!(state2.as_deref(), Some("working"));
    }

    /// Session ID is captured from extracted data.
    #[test]
    fn prop_session_id_captured(pane_id in 0_u64..10_000) {
        let mut correlator = AgentCorrelator::new();
        correlator.ingest_detections(
            pane_id,
            &[make_detection_with_session(
                "core.codex:banner",
                AgentType::Codex,
                "sess-test-123",
            )],
        );

        let meta = correlator.get_metadata(pane_id).unwrap();
        prop_assert_eq!(meta.session_id.as_deref(), Some("sess-test-123"));
    }

    /// remove_pane after ingest reduces tracked count.
    #[test]
    fn prop_remove_pane_after_ingest(pane_id in 0_u64..10_000) {
        let mut correlator = AgentCorrelator::new();
        correlator.ingest_detections(
            pane_id,
            &[make_detection("core.gemini:banner", AgentType::Gemini)],
        );
        prop_assert_eq!(correlator.tracked_pane_count(), 1);

        correlator.remove_pane(pane_id);
        prop_assert_eq!(correlator.tracked_pane_count(), 0);
        prop_assert!(correlator.get_metadata(pane_id).is_none());
    }

    /// remove_pane for a non-existent pane is a no-op on a populated correlator.
    #[test]
    fn prop_remove_nonexistent_noop(pane_id in 0_u64..10_000) {
        let mut correlator = AgentCorrelator::new();
        // Add a different pane
        correlator.ingest_detections(
            pane_id + 1,
            &[make_detection("core.claude_code:banner", AgentType::ClaudeCode)],
        );
        let count_before = correlator.tracked_pane_count();
        correlator.remove_pane(pane_id);
        prop_assert_eq!(correlator.tracked_pane_count(), count_before);
    }
}

// =========================================================================
// AgentCorrelator — update_from_pane_info properties
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// Pane with "claude" in title is detected as ClaudeCode.
    #[test]
    fn prop_title_detects_claude(pane_id in 0_u64..10_000) {
        let mut correlator = AgentCorrelator::new();
        correlator.update_from_pane_info(&make_pane_info(pane_id, Some("claude-code ~/project")));

        let meta = correlator.get_metadata(pane_id);
        prop_assert!(meta.is_some());
        prop_assert_eq!(&meta.unwrap().agent_type, "claude_code");
    }

    /// Pane with "codex" in title is detected as Codex.
    #[test]
    fn prop_title_detects_codex(pane_id in 0_u64..10_000) {
        let mut correlator = AgentCorrelator::new();
        correlator.update_from_pane_info(&make_pane_info(pane_id, Some("codex --model o4-mini")));

        let meta = correlator.get_metadata(pane_id);
        prop_assert!(meta.is_some());
        prop_assert_eq!(&meta.unwrap().agent_type, "codex");
    }

    /// Pane with "gemini" in title is detected as Gemini.
    #[test]
    fn prop_title_detects_gemini(pane_id in 0_u64..10_000) {
        let mut correlator = AgentCorrelator::new();
        correlator.update_from_pane_info(&make_pane_info(pane_id, Some("gemini-cli")));

        let meta = correlator.get_metadata(pane_id);
        prop_assert!(meta.is_some());
        prop_assert_eq!(&meta.unwrap().agent_type, "gemini");
    }

    /// Pane with no agent keyword in title returns None.
    #[test]
    fn prop_title_no_agent(pane_id in 0_u64..10_000) {
        let mut correlator = AgentCorrelator::new();
        correlator.update_from_pane_info(&make_pane_info(pane_id, Some("bash")));
        prop_assert!(correlator.get_metadata(pane_id).is_none());
    }

    /// Pane with None title returns None.
    #[test]
    fn prop_no_title(pane_id in 0_u64..10_000) {
        let mut correlator = AgentCorrelator::new();
        correlator.update_from_pane_info(&make_pane_info(pane_id, None));
        prop_assert!(correlator.get_metadata(pane_id).is_none());
    }

    /// Process name detection: claude process detected as ClaudeCode.
    #[test]
    fn prop_process_detects_claude(pane_id in 0_u64..10_000) {
        let mut correlator = AgentCorrelator::new();
        correlator.update_from_pane_info(&make_pane_info_with_process(pane_id, "claude-code"));

        let meta = correlator.get_metadata(pane_id);
        prop_assert!(meta.is_some());
        prop_assert_eq!(&meta.unwrap().agent_type, "claude_code");
    }

    /// Pattern detection takes priority over pane title.
    #[test]
    fn prop_pattern_priority_over_title(pane_id in 0_u64..10_000) {
        let mut correlator = AgentCorrelator::new();

        // First: detect via patterns
        correlator.ingest_detections(
            pane_id,
            &[make_detection("core.claude_code:tool_use", AgentType::ClaudeCode)],
        );

        // Then: pane info update with a different agent title
        correlator.update_from_pane_info(&make_pane_info(pane_id, Some("gemini-cli")));

        // Should still be ClaudeCode from patterns
        let meta = correlator.get_metadata(pane_id).unwrap();
        prop_assert_eq!(&meta.agent_type, "claude_code");
    }

    /// Title detection sets state to "active".
    #[test]
    fn prop_title_detection_state_is_active(pane_id in 0_u64..10_000) {
        let mut correlator = AgentCorrelator::new();
        correlator.update_from_pane_info(&make_pane_info(pane_id, Some("claude-code ~/project")));

        let meta = correlator.get_metadata(pane_id).unwrap();
        prop_assert_eq!(meta.state.as_deref(), Some("active"));
    }
}

// =========================================================================
// Unit tests
// =========================================================================

#[test]
fn detection_source_variants_distinct() {
    assert_ne!(DetectionSource::PatternEngine, DetectionSource::PaneTitle);
    assert_ne!(DetectionSource::PatternEngine, DetectionSource::ProcessName);
    assert_ne!(DetectionSource::PaneTitle, DetectionSource::ProcessName);
}

#[test]
fn correlator_new_and_default() {
    let a = AgentCorrelator::new();
    let b = AgentCorrelator::default();
    assert_eq!(a.tracked_pane_count(), 0);
    assert_eq!(b.tracked_pane_count(), 0);
}
