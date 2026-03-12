//! Integration tests for the agent detection pipeline (ft-dr6zv.2.5).
//!
//! Validates the full detection-to-inventory flow across:
//! - `AgentCorrelator` detection from patterns, pane titles, and process names
//! - `AgentInventory` assembly with running + installed agent snapshots
//! - Serde roundtrip stability for all inventory types
//! - Priority enforcement (pattern > title > process)
//! - Cross-pane isolation and multi-agent lifecycle
//! - E2E swarm scenarios with heterogeneous agent fleets

use std::collections::{BTreeMap, HashMap};

use frankenterm_core::agent_correlator::{
    AgentCorrelator, AgentInventory, DetectionSource, InstalledAgentInventoryEntry,
    RunningAgentInventoryEntry,
};
use frankenterm_core::patterns::{AgentType, Detection, Severity};
use frankenterm_core::wezterm::PaneInfo;
use serde_json::Value;

// =========================================================================
// Test helpers
// =========================================================================

/// Build a `PaneInfo` with optional title and foreground process name.
fn pane(pane_id: u64, title: Option<&str>, process_name: Option<&str>) -> PaneInfo {
    let mut extra: HashMap<String, Value> = HashMap::new();
    if let Some(proc) = process_name {
        extra.insert(
            "foreground_process_name".to_string(),
            Value::String(proc.to_string()),
        );
    }
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
        title: title.map(std::string::ToString::to_string),
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

/// Build a pattern `Detection` with the given rule_id and agent_type.
fn detection(rule_id: &str, agent_type: AgentType) -> Detection {
    Detection {
        rule_id: rule_id.to_string(),
        agent_type,
        event_type: "test".to_string(),
        severity: Severity::Info,
        confidence: 0.95,
        extracted: serde_json::json!({}),
        matched_text: String::new(),
        span: (0, 0),
    }
}

/// Build a pattern `Detection` with a session ID in extracted data.
fn detection_with_session(
    rule_id: &str,
    agent_type: AgentType,
    session_id: &str,
) -> Detection {
    Detection {
        rule_id: rule_id.to_string(),
        agent_type,
        event_type: "test".to_string(),
        severity: Severity::Info,
        confidence: 0.95,
        extracted: serde_json::json!({"session_id": session_id}),
        matched_text: String::new(),
        span: (0, 0),
    }
}

// =========================================================================
// 1. Full pipeline: detection → correlation → inventory → serde
// =========================================================================

#[test]
fn full_pipeline_detection_to_inventory_roundtrip() {
    let mut correlator = AgentCorrelator::new();

    // Detect Claude Code via pattern on pane 1
    correlator.ingest_detections(
        1,
        &[detection_with_session(
            "core.claude_code:tool_use",
            AgentType::ClaudeCode,
            "cc-sess-42",
        )],
    );

    // Detect Codex via pane title on pane 2
    correlator.update_from_pane_info(&pane(2, Some("codex --model o4-mini"), None));

    // Detect Gemini via process name on pane 3
    correlator.update_from_pane_info(&pane(3, Some("bash"), Some("gemini-cli")));

    // Build inventory snapshot
    let inventory = correlator.inventory();

    // Verify running agents
    assert_eq!(inventory.running.len(), 3);

    let cc = inventory.running.get(&1).expect("pane 1 should have agent");
    assert_eq!(cc.slug, "claude");
    assert_eq!(cc.state, "working");
    assert_eq!(cc.session_id.as_deref(), Some("cc-sess-42"));
    assert_eq!(cc.source, DetectionSource::PatternEngine);

    let codex = inventory.running.get(&2).expect("pane 2 should have agent");
    assert_eq!(codex.slug, "codex");
    assert_eq!(codex.state, "active");
    assert_eq!(codex.source, DetectionSource::PaneTitle);

    let gemini = inventory.running.get(&3).expect("pane 3 should have agent");
    assert_eq!(gemini.slug, "gemini");
    assert_eq!(gemini.state, "active");
    assert_eq!(gemini.source, DetectionSource::ProcessName);

    // Serde roundtrip
    let json = serde_json::to_string(&inventory).unwrap();
    let back: AgentInventory = serde_json::from_str(&json).unwrap();
    assert_eq!(back.running.len(), 3);
    assert_eq!(back.running.get(&1).unwrap().slug, "claude");
    assert_eq!(back.running.get(&2).unwrap().slug, "codex");
    assert_eq!(back.running.get(&3).unwrap().slug, "gemini");
}

// =========================================================================
// 2. Multi-agent lifecycle: detect → state transitions → remove → re-detect
// =========================================================================

#[test]
fn lifecycle_state_transitions_across_pane() {
    let mut correlator = AgentCorrelator::new();
    let pane_id = 10;

    // Phase 1: Agent starts (banner)
    correlator.ingest_detections(
        pane_id,
        &[detection("core.codex:banner", AgentType::Codex)],
    );
    let meta = correlator.get_metadata(pane_id).unwrap();
    assert_eq!(meta.state.as_deref(), Some("starting"));

    // Phase 2: Agent starts working (tool_use)
    correlator.ingest_detections(
        pane_id,
        &[detection("core.codex:tool_use", AgentType::Codex)],
    );
    assert_eq!(
        correlator.get_metadata(pane_id).unwrap().state.as_deref(),
        Some("working")
    );

    // Phase 3: Agent hits rate limit
    correlator.ingest_detections(
        pane_id,
        &[detection("core.codex:usage.reached", AgentType::Codex)],
    );
    assert_eq!(
        correlator.get_metadata(pane_id).unwrap().state.as_deref(),
        Some("rate_limited")
    );

    // Phase 4: Agent becomes idle (cost_summary)
    correlator.ingest_detections(
        pane_id,
        &[detection("core.codex:cost_summary", AgentType::Codex)],
    );
    assert_eq!(
        correlator.get_metadata(pane_id).unwrap().state.as_deref(),
        Some("idle")
    );

    // Phase 5: Pane removed (agent exits)
    correlator.remove_pane(pane_id);
    assert!(correlator.get_metadata(pane_id).is_none());

    // Phase 6: Different agent starts on same pane
    correlator.ingest_detections(
        pane_id,
        &[detection("core.gemini:session.start", AgentType::Gemini)],
    );
    let meta = correlator.get_metadata(pane_id).unwrap();
    assert_eq!(meta.agent_type, "gemini");
    assert_eq!(meta.state.as_deref(), Some("starting"));
}

#[test]
fn lifecycle_approval_and_auth_error_states() {
    let mut correlator = AgentCorrelator::new();

    // Approval needed state
    correlator.ingest_detections(
        1,
        &[detection(
            "core.claude_code:approval_needed",
            AgentType::ClaudeCode,
        )],
    );
    assert_eq!(
        correlator.get_metadata(1).unwrap().state.as_deref(),
        Some("waiting_approval")
    );

    // Auth error state
    correlator.ingest_detections(
        2,
        &[detection(
            "core.codex:auth.api_key_error",
            AgentType::Codex,
        )],
    );
    assert_eq!(
        correlator.get_metadata(2).unwrap().state.as_deref(),
        Some("auth_error")
    );
}

// =========================================================================
// 3. Detection source priority enforcement
// =========================================================================

#[test]
fn pattern_detection_has_priority_over_title() {
    let mut correlator = AgentCorrelator::new();

    // First detect via patterns (highest priority)
    correlator.ingest_detections(
        5,
        &[detection("core.claude_code:tool_use", AgentType::ClaudeCode)],
    );

    // Then try title-based detection for same pane with different agent
    correlator.update_from_pane_info(&pane(5, Some("gemini-cli"), None));

    // Pattern detection should win — still Claude Code
    let meta = correlator.get_metadata(5).unwrap();
    assert_eq!(meta.agent_type, "claude_code");
    assert_eq!(meta.state.as_deref(), Some("working"));
}

#[test]
fn title_detection_has_priority_over_process_name() {
    let mut correlator = AgentCorrelator::new();

    // Title says "codex", process says "gemini-cli"
    correlator.update_from_pane_info(&pane(7, Some("codex session"), Some("gemini-cli")));

    // Title detection runs first and wins
    let meta = correlator.get_metadata(7).unwrap();
    assert_eq!(meta.agent_type, "codex");
    assert_eq!(meta.state.as_deref(), Some("active"));
}

#[test]
fn process_name_detection_when_no_title_match() {
    let mut correlator = AgentCorrelator::new();

    // Title is generic (no agent keywords), but process is gemini
    correlator.update_from_pane_info(&pane(8, Some("bash"), Some("gemini-cli")));

    let meta = correlator.get_metadata(8).unwrap();
    assert_eq!(meta.agent_type, "gemini");
    assert_eq!(
        correlator.inventory().running.get(&8).unwrap().source,
        DetectionSource::ProcessName
    );
}

// =========================================================================
// 4. Inventory running/installed field validation
// =========================================================================

#[test]
fn inventory_running_field_reflects_current_panes() {
    let mut correlator = AgentCorrelator::new();

    correlator.ingest_detections(
        1,
        &[detection("core.claude_code:banner", AgentType::ClaudeCode)],
    );
    correlator.ingest_detections(
        2,
        &[detection("core.codex:tool_use", AgentType::Codex)],
    );

    let inv = correlator.inventory();
    assert_eq!(inv.running.len(), 2);

    // Verify pane 1
    let r1 = &inv.running[&1];
    assert_eq!(r1.slug, "claude");
    assert_eq!(r1.state, "starting");

    // Verify pane 2
    let r2 = &inv.running[&2];
    assert_eq!(r2.slug, "codex");
    assert_eq!(r2.state, "working");

    // Remove pane 1, inventory should update
    correlator.remove_pane(1);
    let inv2 = correlator.inventory();
    assert_eq!(inv2.running.len(), 1);
    assert!(!inv2.running.contains_key(&1));
    assert!(inv2.running.contains_key(&2));
}

#[test]
fn inventory_empty_when_no_agents_detected() {
    let correlator = AgentCorrelator::new();
    let inv = correlator.inventory();
    assert!(inv.running.is_empty());
}

// =========================================================================
// 5. Cross-pane isolation
// =========================================================================

#[test]
fn pane_state_changes_do_not_affect_other_panes() {
    let mut correlator = AgentCorrelator::new();

    // Set up two panes with different agents
    correlator.ingest_detections(
        1,
        &[detection("core.claude_code:banner", AgentType::ClaudeCode)],
    );
    correlator.ingest_detections(
        2,
        &[detection("core.codex:tool_use", AgentType::Codex)],
    );

    // Update pane 1 state
    correlator.ingest_detections(
        1,
        &[detection(
            "core.claude_code:rate_limited",
            AgentType::ClaudeCode,
        )],
    );

    // Pane 2 should be unaffected
    let meta2 = correlator.get_metadata(2).unwrap();
    assert_eq!(meta2.agent_type, "codex");
    assert_eq!(meta2.state.as_deref(), Some("working"));

    // Remove pane 2 should not affect pane 1
    correlator.remove_pane(2);
    let meta1 = correlator.get_metadata(1).unwrap();
    assert_eq!(meta1.agent_type, "claude_code");
    assert_eq!(meta1.state.as_deref(), Some("rate_limited"));
    assert_eq!(correlator.tracked_pane_count(), 1);
}

// =========================================================================
// 6. Serde roundtrip stability for inventory types
// =========================================================================

#[test]
fn running_agent_inventory_entry_serde_roundtrip() {
    let entry = RunningAgentInventoryEntry {
        slug: "claude".to_string(),
        state: "working".to_string(),
        session_id: Some("sess-42".to_string()),
        source: DetectionSource::PatternEngine,
    };

    let json = serde_json::to_string(&entry).unwrap();
    let back: RunningAgentInventoryEntry = serde_json::from_str(&json).unwrap();
    assert_eq!(entry, back);
}

#[test]
fn running_agent_inventory_entry_without_session_id() {
    let entry = RunningAgentInventoryEntry {
        slug: "codex".to_string(),
        state: "active".to_string(),
        session_id: None,
        source: DetectionSource::PaneTitle,
    };

    let json = serde_json::to_string(&entry).unwrap();
    let back: RunningAgentInventoryEntry = serde_json::from_str(&json).unwrap();
    assert_eq!(entry, back);

    // Verify null is present in JSON
    let val: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(val["session_id"].is_null());
}

#[test]
fn installed_agent_inventory_entry_serde_roundtrip() {
    let entry = InstalledAgentInventoryEntry {
        slug: "claude".to_string(),
        detected: true,
        evidence: vec![
            "Found ~/.claude/config.json".to_string(),
            "Version: 1.2.3".to_string(),
        ],
        root_paths: vec!["~/.claude".to_string()],
        config_path: Some("~/.claude/config.json".to_string()),
        binary_path: Some("/usr/local/bin/claude".to_string()),
        version: Some("1.2.3".to_string()),
    };

    let json = serde_json::to_string(&entry).unwrap();
    let back: InstalledAgentInventoryEntry = serde_json::from_str(&json).unwrap();
    assert_eq!(entry, back);
}

#[test]
fn installed_agent_inventory_entry_minimal() {
    let entry = InstalledAgentInventoryEntry {
        slug: "aider".to_string(),
        detected: false,
        evidence: vec![],
        root_paths: vec![],
        config_path: None,
        binary_path: None,
        version: None,
    };

    let json = serde_json::to_string(&entry).unwrap();
    let back: InstalledAgentInventoryEntry = serde_json::from_str(&json).unwrap();
    assert_eq!(entry, back);
}

#[test]
fn agent_inventory_serde_roundtrip_with_running_agents() {
    let mut running = BTreeMap::new();
    running.insert(
        1,
        RunningAgentInventoryEntry {
            slug: "claude".to_string(),
            state: "working".to_string(),
            session_id: Some("sess-1".to_string()),
            source: DetectionSource::PatternEngine,
        },
    );
    running.insert(
        2,
        RunningAgentInventoryEntry {
            slug: "codex".to_string(),
            state: "idle".to_string(),
            session_id: None,
            source: DetectionSource::PaneTitle,
        },
    );

    let inventory = AgentInventory {
        installed: vec![InstalledAgentInventoryEntry {
            slug: "claude".to_string(),
            detected: true,
            evidence: vec!["Found config".to_string()],
            root_paths: vec!["~/.claude".to_string()],
            config_path: Some("~/.claude/config.json".to_string()),
            binary_path: None,
            version: Some("2.0".to_string()),
        }],
        running,
    };

    let json = serde_json::to_string(&inventory).unwrap();
    let back: AgentInventory = serde_json::from_str(&json).unwrap();
    assert_eq!(back.installed.len(), 1);
    assert_eq!(back.running.len(), 2);
    assert_eq!(back.installed[0].slug, "claude");
    assert_eq!(back.running[&1].slug, "claude");
    assert_eq!(back.running[&2].slug, "codex");
}

#[test]
fn agent_inventory_default_is_empty() {
    let inv = AgentInventory::default();
    assert!(inv.installed.is_empty());
    assert!(inv.running.is_empty());

    // Default roundtrips cleanly
    let json = serde_json::to_string(&inv).unwrap();
    let back: AgentInventory = serde_json::from_str(&json).unwrap();
    assert!(back.installed.is_empty());
    assert!(back.running.is_empty());
}

// =========================================================================
// 7. Feature flag behavior
// =========================================================================

#[test]
fn filesystem_detection_available_reflects_feature_flag() {
    let available = frankenterm_core::agent_correlator::filesystem_detection_available();
    // When agent-detection feature is enabled (default), should be true
    // When disabled, should be false — this test validates the API exists
    // and returns a consistent boolean
    assert_eq!(available, cfg!(feature = "agent-detection"));
}

// =========================================================================
// 8. E2E swarm scenario: multi-agent fleet with lifecycle events
// =========================================================================

#[test]
fn e2e_swarm_scenario_heterogeneous_fleet() {
    let mut correlator = AgentCorrelator::new();

    // --- Setup phase: 5 agents across 5 panes ---

    // Pane 0: Claude Code detected via pattern
    correlator.ingest_detections(
        0,
        &[detection_with_session(
            "core.claude_code:banner",
            AgentType::ClaudeCode,
            "cc-main-session",
        )],
    );

    // Pane 1: Codex detected via pattern
    correlator.ingest_detections(
        1,
        &[detection("core.codex:session.start", AgentType::Codex)],
    );

    // Pane 2: Gemini detected via pane title
    correlator.update_from_pane_info(&pane(2, Some("gemini-cli ~/project"), None));

    // Pane 3: Claude Code detected via process name
    correlator.update_from_pane_info(&pane(3, Some("zsh"), Some("claude-code")));

    // Pane 4: Non-agent pane (bash, no agent keywords)
    correlator.update_from_pane_info(&pane(4, Some("bash"), Some("vim")));

    // Verify initial state
    assert_eq!(correlator.tracked_pane_count(), 4); // pane 4 not tracked
    assert!(correlator.get_metadata(4).is_none());

    let inv = correlator.inventory();
    assert_eq!(inv.running.len(), 4);

    // --- Work phase: agents start producing output ---

    correlator.ingest_detections(
        0,
        &[detection("core.claude_code:tool_use", AgentType::ClaudeCode)],
    );
    correlator.ingest_detections(
        1,
        &[detection("core.codex:tool_use", AgentType::Codex)],
    );

    // Verify working states
    assert_eq!(
        correlator.get_metadata(0).unwrap().state.as_deref(),
        Some("working")
    );
    assert_eq!(
        correlator.get_metadata(1).unwrap().state.as_deref(),
        Some("working")
    );
    // Title/process detected agents stay "active" (no state updates from detections)
    assert_eq!(
        correlator.get_metadata(2).unwrap().state.as_deref(),
        Some("active")
    );

    // Session ID preserved through state updates
    assert_eq!(
        correlator
            .get_metadata(0)
            .unwrap()
            .session_id
            .as_deref(),
        Some("cc-main-session")
    );

    // --- Incident phase: rate limits and auth errors ---

    correlator.ingest_detections(
        0,
        &[detection(
            "core.claude_code:usage.reached",
            AgentType::ClaudeCode,
        )],
    );
    correlator.ingest_detections(
        1,
        &[detection("core.codex:auth.device_code_prompt", AgentType::Codex)],
    );

    assert_eq!(
        correlator.get_metadata(0).unwrap().state.as_deref(),
        Some("rate_limited")
    );
    assert_eq!(
        correlator.get_metadata(1).unwrap().state.as_deref(),
        Some("auth_error")
    );

    // --- Recovery phase: agent recovers and completes ---

    correlator.ingest_detections(
        0,
        &[detection("core.claude_code:tool_use", AgentType::ClaudeCode)],
    );
    assert_eq!(
        correlator.get_metadata(0).unwrap().state.as_deref(),
        Some("working")
    );

    correlator.ingest_detections(
        1,
        &[detection("core.codex:cost_summary", AgentType::Codex)],
    );
    assert_eq!(
        correlator.get_metadata(1).unwrap().state.as_deref(),
        Some("idle")
    );

    // --- Teardown phase: agents exit ---

    correlator.remove_pane(0);
    correlator.remove_pane(1);
    assert_eq!(correlator.tracked_pane_count(), 2); // panes 2 and 3 remain

    let inv = correlator.inventory();
    assert_eq!(inv.running.len(), 2);
    assert!(inv.running.contains_key(&2));
    assert!(inv.running.contains_key(&3));
}

#[test]
fn e2e_swarm_scenario_agent_replacement() {
    let mut correlator = AgentCorrelator::new();

    // Agent starts on pane 0
    correlator.ingest_detections(
        0,
        &[detection("core.codex:banner", AgentType::Codex)],
    );
    assert_eq!(correlator.get_metadata(0).unwrap().agent_type, "codex");

    // Agent exits
    correlator.remove_pane(0);

    // Different agent starts on same pane
    correlator.ingest_detections(
        0,
        &[detection(
            "core.claude_code:session.start",
            AgentType::ClaudeCode,
        )],
    );
    let meta = correlator.get_metadata(0).unwrap();
    assert_eq!(meta.agent_type, "claude_code");
    assert_eq!(meta.state.as_deref(), Some("starting"));
}

// =========================================================================
// 9. Inventory JSON schema stability
// =========================================================================

#[test]
fn inventory_json_contains_expected_fields() {
    let mut correlator = AgentCorrelator::new();
    correlator.ingest_detections(
        1,
        &[detection_with_session(
            "core.codex:tool_use",
            AgentType::Codex,
            "s-123",
        )],
    );

    let inv = correlator.inventory();
    let json: serde_json::Value = serde_json::to_value(&inv).unwrap();

    // Top-level fields
    assert!(json.get("installed").is_some());
    assert!(json.get("running").is_some());

    // Running entry has expected shape
    let running_1 = &json["running"]["1"];
    assert_eq!(running_1["slug"], "codex");
    assert_eq!(running_1["state"], "working");
    assert_eq!(running_1["session_id"], "s-123");
    assert_eq!(running_1["source"], "pattern_engine");
}

#[test]
fn detection_source_serializes_to_snake_case() {
    assert_eq!(
        serde_json::to_value(DetectionSource::PatternEngine).unwrap(),
        "pattern_engine"
    );
    assert_eq!(
        serde_json::to_value(DetectionSource::PaneTitle).unwrap(),
        "pane_title"
    );
    assert_eq!(
        serde_json::to_value(DetectionSource::ProcessName).unwrap(),
        "process_name"
    );
}

// =========================================================================
// 10. Edge cases and robustness
// =========================================================================

#[test]
fn wezterm_and_unknown_agents_filtered_from_inventory() {
    let mut correlator = AgentCorrelator::new();
    correlator.ingest_detections(
        1,
        &[detection("core.wezterm:mux.event", AgentType::Wezterm)],
    );
    correlator.ingest_detections(
        2,
        &[detection("unknown:event", AgentType::Unknown)],
    );

    assert_eq!(correlator.tracked_pane_count(), 0);
    assert!(correlator.inventory().running.is_empty());
}

#[test]
fn empty_detection_batch_is_noop() {
    let mut correlator = AgentCorrelator::new();
    correlator.ingest_detections(1, &[]);
    assert_eq!(correlator.tracked_pane_count(), 0);
}

#[test]
fn pane_with_no_agent_keywords_not_tracked() {
    let mut correlator = AgentCorrelator::new();
    correlator.update_from_pane_info(&pane(1, Some("htop"), None));
    correlator.update_from_pane_info(&pane(2, Some("vim main.rs"), None));
    correlator.update_from_pane_info(&pane(3, None, Some("python3")));
    assert_eq!(correlator.tracked_pane_count(), 0);
}

#[test]
fn multiple_detections_in_single_batch_uses_last_state() {
    let mut correlator = AgentCorrelator::new();
    correlator.ingest_detections(
        1,
        &[
            detection("core.codex:banner", AgentType::Codex),
            detection("core.codex:tool_use", AgentType::Codex),
            detection("core.codex:usage.reached", AgentType::Codex),
        ],
    );

    // Last detection in batch wins for state
    assert_eq!(
        correlator.get_metadata(1).unwrap().state.as_deref(),
        Some("rate_limited")
    );
}

#[test]
fn session_id_preserved_when_new_detection_has_none() {
    let mut correlator = AgentCorrelator::new();

    // First detection with session ID
    correlator.ingest_detections(
        1,
        &[detection_with_session(
            "core.codex:banner",
            AgentType::Codex,
            "original-session",
        )],
    );

    // Second detection without session ID should preserve original
    correlator.ingest_detections(
        1,
        &[detection("core.codex:tool_use", AgentType::Codex)],
    );

    assert_eq!(
        correlator
            .get_metadata(1)
            .unwrap()
            .session_id
            .as_deref(),
        Some("original-session")
    );
}

#[test]
fn session_id_updated_when_new_session_provided() {
    let mut correlator = AgentCorrelator::new();

    correlator.ingest_detections(
        1,
        &[detection_with_session(
            "core.codex:banner",
            AgentType::Codex,
            "sess-1",
        )],
    );

    correlator.ingest_detections(
        1,
        &[detection_with_session(
            "core.codex:tool_use",
            AgentType::Codex,
            "sess-2",
        )],
    );

    assert_eq!(
        correlator
            .get_metadata(1)
            .unwrap()
            .session_id
            .as_deref(),
        Some("sess-2")
    );
}

#[test]
fn remove_nonexistent_pane_is_noop() {
    let mut correlator = AgentCorrelator::new();
    correlator.remove_pane(999); // Should not panic
    assert_eq!(correlator.tracked_pane_count(), 0);
}

#[test]
fn large_pane_id_space() {
    let mut correlator = AgentCorrelator::new();
    let large_id = u64::MAX - 1;

    correlator.ingest_detections(
        large_id,
        &[detection("core.codex:banner", AgentType::Codex)],
    );

    let meta = correlator.get_metadata(large_id).unwrap();
    assert_eq!(meta.agent_type, "codex");

    let inv = correlator.inventory();
    assert!(inv.running.contains_key(&large_id));
}

// =========================================================================
// 11. Metadata contract validation
// =========================================================================

#[test]
fn metadata_agent_type_field_uses_legacy_strings() {
    let mut correlator = AgentCorrelator::new();

    correlator.ingest_detections(
        1,
        &[detection("core.claude_code:banner", AgentType::ClaudeCode)],
    );
    correlator.ingest_detections(
        2,
        &[detection("core.codex:banner", AgentType::Codex)],
    );
    correlator.ingest_detections(
        3,
        &[detection("core.gemini:banner", AgentType::Gemini)],
    );

    assert_eq!(correlator.get_metadata(1).unwrap().agent_type, "claude_code");
    assert_eq!(correlator.get_metadata(2).unwrap().agent_type, "codex");
    assert_eq!(correlator.get_metadata(3).unwrap().agent_type, "gemini");
}

#[test]
fn metadata_state_defaults_to_active_for_unknown_rules() {
    let mut correlator = AgentCorrelator::new();
    correlator.ingest_detections(
        1,
        &[detection(
            "core.claude_code:some_future_event",
            AgentType::ClaudeCode,
        )],
    );
    assert_eq!(
        correlator.get_metadata(1).unwrap().state.as_deref(),
        Some("active")
    );
}

// =========================================================================
// 12. Correlator default trait
// =========================================================================

#[test]
fn correlator_implements_default() {
    let c = AgentCorrelator::default();
    assert_eq!(c.tracked_pane_count(), 0);
    assert!(c.inventory().running.is_empty());
}

// =========================================================================
// 13. Cross-detection-source consistency in inventory
// =========================================================================

#[test]
fn inventory_source_field_reflects_detection_method() {
    let mut correlator = AgentCorrelator::new();

    // Pattern-detected
    correlator.ingest_detections(
        1,
        &[detection("core.claude_code:banner", AgentType::ClaudeCode)],
    );

    // Title-detected
    correlator.update_from_pane_info(&pane(2, Some("codex session"), None));

    // Process-detected
    correlator.update_from_pane_info(&pane(3, Some("bash"), Some("gemini-cli")));

    let inv = correlator.inventory();
    assert_eq!(inv.running[&1].source, DetectionSource::PatternEngine);
    assert_eq!(inv.running[&2].source, DetectionSource::PaneTitle);
    assert_eq!(inv.running[&3].source, DetectionSource::ProcessName);
}
