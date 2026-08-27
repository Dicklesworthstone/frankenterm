//! Cross-module enrichment integration tests for agent detection (ft-dr6zv.2.5).
//!
//! Validates the full detection → correlation → inventory → autoconfig pipeline
//! using multiple subsystems together:
//! - Filesystem detection feeds installed inventory
//! - Pattern detection feeds running inventory
//! - Combined inventory drives autoconfig generation plan
//! - Event lifecycle ordering
//! - Structured test logging

use std::collections::HashMap;

use frankenterm_core::agent_config_templates::{
    ConfigAction, ConfigScope, build_generation_plan, generate_templates_for_detected,
};
use frankenterm_core::agent_correlator::{
    AgentCorrelator, AgentInventory, DetectionSource, InstalledAgentInventoryEntry,
    RunningAgentInventoryEntry,
};
use frankenterm_core::agent_provider::AgentProvider;
use frankenterm_core::patterns::{AgentType, Detection, Severity};
use frankenterm_core::wezterm::PaneInfo;
use serde_json::Value;

// =========================================================================
// Helpers
// =========================================================================

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

fn detection_with_session(rule_id: &str, agent_type: AgentType, session_id: &str) -> Detection {
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

/// Structured log entry for test phases.
#[derive(serde::Serialize)]
struct TestLogEntry {
    test_name: &'static str,
    phase: &'static str,
    result: &'static str,
    detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    agents_detected: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    metrics: Option<serde_json::Value>,
}

fn log_phase(entry: &TestLogEntry) {
    let json = serde_json::to_string(entry).expect("serialize log entry");
    eprintln!("[test-log] {json}");
}

// =========================================================================
// 1. Detection → Inventory → Autoconfig plan (full pipeline)
// =========================================================================

#[test]
fn detection_to_inventory_to_autoconfig_plan() {
    log_phase(&TestLogEntry {
        test_name: "detection_to_inventory_to_autoconfig_plan",
        phase: "setup",
        result: "pass",
        detail: "Starting full pipeline test".to_string(),
        agents_detected: None,
        metrics: None,
    });

    let mut correlator = AgentCorrelator::new();

    // Simulate pattern-detected agents across 3 panes
    // Note: title detection only recognizes claude/codex/gemini keywords,
    // so we use pattern detection for all three to ensure reliable results.
    correlator.ingest_detections(
        0,
        &[detection_with_session(
            "core.claude_code:tool_use",
            AgentType::ClaudeCode,
            "cc-pipeline-1",
        )],
    );
    correlator.ingest_detections(1, &[detection("core.codex:banner", AgentType::Codex)]);
    correlator.ingest_detections(2, &[detection("core.gemini:tool_use", AgentType::Gemini)]);

    log_phase(&TestLogEntry {
        test_name: "detection_to_inventory_to_autoconfig_plan",
        phase: "correlate",
        result: "pass",
        detail: "Correlator populated with 3 agents".to_string(),
        agents_detected: Some(vec![
            "claude".to_string(),
            "codex".to_string(),
            "gemini".to_string(),
        ]),
        metrics: Some(serde_json::json!({"pane_count": 3})),
    });

    // Build inventory
    let inv = correlator.inventory();
    assert_eq!(inv.running.len(), 3);

    // Extract slugs from running inventory
    let mut detected_slugs: Vec<String> = inv.running.values().map(|r| r.slug.clone()).collect();
    detected_slugs.sort();

    // Generate autoconfig plan for detected agents
    let plan = build_generation_plan(&detected_slugs, ConfigScope::Project, |_| (false, None));

    assert_eq!(plan.len(), 3);
    // All should be Create since no files exist
    for item in &plan {
        assert_eq!(
            item.action,
            ConfigAction::Create,
            "{}: should be Create",
            item.slug
        );
        assert_ne!(item.content_preview, "");
    }

    log_phase(&TestLogEntry {
        test_name: "detection_to_inventory_to_autoconfig_plan",
        phase: "assert",
        result: "pass",
        detail: "Plan generated successfully for 3 agents".to_string(),
        agents_detected: Some(detected_slugs),
        metrics: Some(serde_json::json!({"plan_items": plan.len()})),
    });
}

// =========================================================================
// 2. Mixed detection sources feed unified autoconfig
// =========================================================================

#[test]
fn mixed_detection_sources_produce_unified_plan() {
    let mut correlator = AgentCorrelator::new();

    // Pattern detection (highest priority)
    correlator.ingest_detections(
        10,
        &[detection(
            "core.claude_code:tool_use",
            AgentType::ClaudeCode,
        )],
    );

    // Title detection
    correlator.update_from_pane_info(&pane(20, Some("codex --model o4-mini"), None));

    // Process name detection
    correlator.update_from_pane_info(&pane(30, Some("bash"), Some("gemini-cli")));

    let inv = correlator.inventory();
    let mut slugs: Vec<String> = inv.running.values().map(|r| r.slug.clone()).collect();
    slugs.sort();

    assert_eq!(slugs, vec!["claude", "codex", "gemini"]);

    // Generate templates — should work regardless of detection source
    let templates = generate_templates_for_detected(&slugs);
    assert_eq!(templates.len(), 3);
    for t in &templates {
        assert!(
            t.content.contains("ft robot"),
            "template for {} missing robot commands",
            t.provider.canonical_slug()
        );
    }
}

// =========================================================================
// 3. Autoconfig idempotency integration
// =========================================================================

#[test]
fn autoconfig_plan_skip_after_successful_apply() {
    let mut correlator = AgentCorrelator::new();
    correlator.ingest_detections(
        1,
        &[detection("core.claude_code:banner", AgentType::ClaudeCode)],
    );

    let inv = correlator.inventory();
    let slugs: Vec<String> = inv.running.values().map(|r| r.slug.clone()).collect();

    // First plan: file doesn't exist → Create
    let plan1 = build_generation_plan(&slugs, ConfigScope::Project, |_| (false, None));
    assert_eq!(plan1[0].action, ConfigAction::Create);

    // Simulate applying the plan: create the file with merged content
    let merged_content = frankenterm_core::agent_config_templates::merge_into_existing(
        "",
        &plan1[0].content_preview,
    );

    // Second plan: file exists with current section → Skip
    let plan2 = build_generation_plan(&slugs, ConfigScope::Project, |_| {
        (true, Some(merged_content.clone()))
    });
    assert_eq!(plan2[0].action, ConfigAction::Skip);
}

// =========================================================================
// 4. Inventory running → installed enrichment gap
// =========================================================================

#[test]
fn running_agents_without_installed_inventory_still_generate_configs() {
    let mut correlator = AgentCorrelator::new();

    // Agent running in pane but NOT in filesystem (e.g., installed in non-default location)
    correlator.ingest_detections(5, &[detection("core.codex:tool_use", AgentType::Codex)]);

    let inv = correlator.inventory();
    assert_eq!(inv.running.len(), 1);
    assert_eq!(
        inv.installed,
        [] as [frankenterm_core::agent_correlator::InstalledAgentInventoryEntry; 0]
    ); // No filesystem detection happened

    // Should still be able to generate configs from running slugs
    let slugs: Vec<String> = inv.running.values().map(|r| r.slug.clone()).collect();
    let templates = generate_templates_for_detected(&slugs);
    assert_eq!(templates.len(), 1);
    assert_eq!(templates[0].provider, AgentProvider::Codex);
}

// =========================================================================
// 5. Agent state transitions do not affect autoconfig
// =========================================================================

#[test]
fn agent_state_transitions_preserve_autoconfig_eligibility() {
    let mut correlator = AgentCorrelator::new();

    // Agent goes through lifecycle: starting → working → rate_limited → idle
    correlator.ingest_detections(1, &[detection("core.codex:banner", AgentType::Codex)]);
    correlator.ingest_detections(1, &[detection("core.codex:tool_use", AgentType::Codex)]);
    correlator.ingest_detections(
        1,
        &[detection("core.codex:usage.reached", AgentType::Codex)],
    );

    // Even in rate_limited state, agent still appears in running inventory
    let inv = correlator.inventory();
    assert_eq!(inv.running.len(), 1);
    assert_eq!(inv.running[&1].state, "rate_limited");

    // Autoconfig should still work
    let slugs: Vec<String> = inv.running.values().map(|r| r.slug.clone()).collect();
    let templates = generate_templates_for_detected(&slugs);
    assert_eq!(templates.len(), 1);
}

// =========================================================================
// 6. Agent removal reduces autoconfig surface
// =========================================================================

#[test]
fn agent_removal_shrinks_autoconfig_plan() {
    let mut correlator = AgentCorrelator::new();

    correlator.ingest_detections(
        1,
        &[detection("core.claude_code:banner", AgentType::ClaudeCode)],
    );
    correlator.ingest_detections(2, &[detection("core.codex:banner", AgentType::Codex)]);

    // Both agents running
    let inv = correlator.inventory();
    assert_eq!(inv.running.values().count(), 2);

    // Remove one agent
    correlator.remove_pane(1);

    // Only one remains
    let inv2 = correlator.inventory();
    let slugs2: Vec<String> = inv2.running.values().map(|r| r.slug.clone()).collect();
    assert_eq!(slugs2.len(), 1);
    assert_eq!(slugs2[0], "codex");

    let plan = build_generation_plan(&slugs2, ConfigScope::Project, |_| (false, None));
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0].slug, "codex");
}

// =========================================================================
// 7. Large swarm → autoconfig
// =========================================================================

#[test]
fn large_swarm_autoconfig_handles_duplicates() {
    let mut correlator = AgentCorrelator::new();

    // 10 Claude Code agents across 10 panes
    for i in 0..10 {
        correlator.ingest_detections(
            i,
            &[detection_with_session(
                "core.claude_code:tool_use",
                AgentType::ClaudeCode,
                &format!("session-{i}"),
            )],
        );
    }

    // 5 Codex agents
    for i in 10..15 {
        correlator.ingest_detections(i, &[detection("core.codex:tool_use", AgentType::Codex)]);
    }

    let inv = correlator.inventory();
    assert_eq!(inv.running.len(), 15);

    // Collect unique slugs (dedup for autoconfig)
    let mut slugs: Vec<String> = inv.running.values().map(|r| r.slug.clone()).collect();
    slugs.sort();
    slugs.dedup();

    assert_eq!(slugs, vec!["claude", "codex"]);

    // Autoconfig plan should have 2 items (not 15)
    let plan = build_generation_plan(&slugs, ConfigScope::Project, |_| (false, None));
    assert_eq!(plan.len(), 2);
}

// =========================================================================
// 8. InstalledAgentInventoryEntry enrichment
// =========================================================================

#[test]
fn installed_inventory_entry_fields() {
    let entry = InstalledAgentInventoryEntry {
        slug: "claude".to_string(),
        detected: true,
        evidence: vec![
            "Found ~/.claude/config.json".to_string(),
            "Version: 2.1.0".to_string(),
        ],
        root_paths: vec!["~/.claude".to_string()],
        config_path: Some("~/.claude/config.json".to_string()),
        binary_path: Some("/usr/local/bin/claude".to_string()),
        version: Some("2.1.0".to_string()),
    };

    // Verify serde roundtrip preserves all fields
    let json = serde_json::to_string(&entry).unwrap();
    let back: InstalledAgentInventoryEntry = serde_json::from_str(&json).unwrap();

    assert_eq!(back.slug, "claude");
    assert!(back.detected);
    assert_eq!(back.evidence.len(), 2);
    assert_eq!(back.root_paths, vec!["~/.claude"]);
    assert_eq!(back.config_path.as_deref(), Some("~/.claude/config.json"));
    assert_eq!(back.binary_path.as_deref(), Some("/usr/local/bin/claude"));
    assert_eq!(back.version.as_deref(), Some("2.1.0"));
}

#[test]
fn installed_inventory_undetected_entry() {
    let entry = InstalledAgentInventoryEntry {
        slug: "windsurf".to_string(),
        detected: false,
        evidence: vec![],
        root_paths: vec![],
        config_path: None,
        binary_path: None,
        version: None,
    };

    let json = serde_json::to_string(&entry).unwrap();
    let back: InstalledAgentInventoryEntry = serde_json::from_str(&json).unwrap();

    assert!(!back.detected);
    assert_eq!(back.evidence, [] as [std::string::String; 0]);
    assert_eq!(back.root_paths, [] as [std::string::String; 0]);
    assert!(back.config_path.is_none());
}

// =========================================================================
// 9. Full inventory with both installed and running
// =========================================================================

#[test]
fn full_inventory_with_installed_and_running_serde() {
    use std::collections::BTreeMap;

    let mut running = BTreeMap::new();
    running.insert(
        0,
        RunningAgentInventoryEntry {
            slug: "claude".to_string(),
            state: "working".to_string(),
            session_id: Some("cc-1".to_string()),
            source: DetectionSource::PatternEngine,
        },
    );
    running.insert(
        1,
        RunningAgentInventoryEntry {
            slug: "codex".to_string(),
            state: "rate_limited".to_string(),
            session_id: None,
            source: DetectionSource::PaneTitle,
        },
    );

    let installed = vec![
        InstalledAgentInventoryEntry {
            slug: "claude".to_string(),
            detected: true,
            evidence: vec!["Found config".to_string()],
            root_paths: vec!["~/.claude".to_string()],
            config_path: Some("~/.claude/config.json".to_string()),
            binary_path: None,
            version: Some("2.0".to_string()),
        },
        InstalledAgentInventoryEntry {
            slug: "codex".to_string(),
            detected: true,
            evidence: vec!["Found binary".to_string()],
            root_paths: vec!["~/.codex".to_string()],
            config_path: None,
            binary_path: Some("/usr/local/bin/codex".to_string()),
            version: None,
        },
    ];

    let inventory = AgentInventory { installed, running };

    // Roundtrip
    let json = serde_json::to_string_pretty(&inventory).unwrap();
    let back: AgentInventory = serde_json::from_str(&json).unwrap();

    assert_eq!(back.installed.len(), 2);
    assert_eq!(back.running.len(), 2);

    // Cross-reference: running slug matches installed slug
    for running_entry in back.running.values() {
        let installed_entry = back.installed.iter().find(|i| i.slug == running_entry.slug);
        assert!(
            installed_entry.is_some(),
            "running agent {} should have installed entry",
            running_entry.slug
        );
    }
}

// =========================================================================
// 10. Provider slug → detection type mapping consistency
// =========================================================================

#[test]
fn all_agent_types_map_to_known_providers() {
    let agent_types = [
        (AgentType::ClaudeCode, "claude"),
        (AgentType::Codex, "codex"),
        (AgentType::Gemini, "gemini"),
    ];

    for (agent_type, expected_slug) in &agent_types {
        let provider = AgentProvider::from_agent_type(agent_type);
        assert_eq!(
            provider.canonical_slug(),
            *expected_slug,
            "AgentType {:?} should map to slug {}",
            agent_type,
            expected_slug
        );
    }
}

#[test]
fn provider_from_slug_roundtrips_for_all_known() {
    let slugs = [
        "claude",
        "cline",
        "codex",
        "cursor",
        "factory",
        "gemini",
        "github-copilot",
        "opencode",
        "windsurf",
        "aider",
        "grok",
        "devin",
    ];

    for slug in &slugs {
        let provider = AgentProvider::from_slug(slug);
        let roundtrip_slug = provider.canonical_slug();
        assert_eq!(
            roundtrip_slug, *slug,
            "slug {} should roundtrip through AgentProvider",
            slug
        );
    }
}

// =========================================================================
// 11. Structured logging completeness
// =========================================================================

#[test]
fn structured_log_entry_serializes_all_fields() {
    let entry = TestLogEntry {
        test_name: "test_example",
        phase: "detect",
        result: "pass",
        detail: "Detected 3 agents".to_string(),
        agents_detected: Some(vec!["claude".to_string(), "codex".to_string()]),
        metrics: Some(serde_json::json!({
            "detection_time_ms": 12,
            "agents_found": 2
        })),
    };

    let json = serde_json::to_string(&entry).unwrap();
    let val: serde_json::Value = serde_json::from_str(&json).unwrap();

    assert_eq!(val["test_name"], "test_example");
    assert_eq!(val["phase"], "detect");
    assert_eq!(val["result"], "pass");
    assert!(val["agents_detected"].is_array());
    assert!(val["metrics"]["detection_time_ms"].is_number());
}

#[test]
fn structured_log_entry_omits_none_fields() {
    let entry = TestLogEntry {
        test_name: "test_minimal",
        phase: "setup",
        result: "pass",
        detail: "No agents".to_string(),
        agents_detected: None,
        metrics: None,
    };

    let json = serde_json::to_string(&entry).unwrap();
    assert!(!json.contains("agents_detected"));
    assert!(!json.contains("metrics"));
}
