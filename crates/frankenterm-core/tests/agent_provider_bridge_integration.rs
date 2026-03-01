//! Integration tests for unified provider identity bridges.
//!
//! These tests validate end-to-end consistency across:
//! - `agent_provider::AgentProvider`
//! - `agent_correlator::AgentCorrelator`
//! - `cass::CassAgent`
//! - `caut::CautService`

use std::collections::HashMap;

use frankenterm_core::agent_correlator::AgentCorrelator;
use frankenterm_core::agent_provider::{AgentProvider, ProviderResolutionSource};
use frankenterm_core::cass::CassAgent;
use frankenterm_core::caut::CautService;
use frankenterm_core::wezterm::PaneInfo;
use serde_json::Value;

fn pane_with_detection_hints(
    pane_id: u64,
    title: Option<&str>,
    process_name: Option<&str>,
) -> PaneInfo {
    let mut extra: HashMap<String, Value> = HashMap::new();
    if let Some(process_name) = process_name {
        extra.insert(
            "foreground_process_name".to_string(),
            Value::String(process_name.to_string()),
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

#[test]
fn ambiguous_process_resolution_stays_deterministic_across_bridges() {
    let diagnostics = AgentProvider::diagnostics_from_process_name("claude-codex-wrapper");
    assert_eq!(diagnostics.source, ProviderResolutionSource::ProcessName);
    assert_eq!(
        diagnostics.candidates,
        vec![AgentProvider::Claude, AgentProvider::Codex]
    );
    assert_eq!(diagnostics.selected, Some(AgentProvider::Claude));
    assert!(diagnostics.ambiguous);

    let mut correlator = AgentCorrelator::new();
    correlator.update_from_pane_info(&pane_with_detection_hints(
        10,
        None,
        Some("claude-codex-wrapper"),
    ));

    let metadata = correlator
        .get_metadata(10)
        .expect("pane should map via deterministic first candidate");
    assert_eq!(metadata.agent_type, "claude_code");

    let inventory = correlator.inventory();
    let running = inventory
        .running
        .get(&10)
        .expect("running inventory should include pane");
    assert_eq!(running.slug, "claude");

    let provider = AgentProvider::from_slug(&running.slug);
    assert_eq!(
        CassAgent::from_provider(&provider),
        Some(CassAgent::ClaudeCode)
    );
    assert_eq!(
        CautService::from_provider(&provider),
        Some(CautService::Anthropic)
    );
}

#[test]
fn openai_alias_slug_maps_consistently_between_caut_and_cass() {
    let provider = AgentProvider::from_slug("chat-gpt");
    assert_eq!(
        CassAgent::from_provider(&provider),
        Some(CassAgent::ChatGpt)
    );
    assert_eq!(CassAgent::from_slug("chat-gpt"), Some(CassAgent::ChatGpt));
    assert_eq!(
        CautService::from_provider(&provider),
        Some(CautService::OpenAI)
    );

    let normalized_from_cass = CassAgent::ChatGpt.to_provider();
    assert_eq!(
        CautService::from_provider(&normalized_from_cass),
        Some(CautService::OpenAI)
    );
}

#[test]
fn correlator_inventory_slug_roundtrips_to_bridge_mappings() {
    let mut correlator = AgentCorrelator::new();
    correlator.update_from_pane_info(&pane_with_detection_hints(
        42,
        Some("codex --model o4-mini"),
        None,
    ));

    let metadata = correlator
        .get_metadata(42)
        .expect("codex title should produce metadata");
    assert_eq!(metadata.agent_type, "codex");

    let inventory = correlator.inventory();
    let running = inventory
        .running
        .get(&42)
        .expect("running inventory should include codex pane");
    assert_eq!(running.slug, "codex");

    let provider = AgentProvider::from_slug(&running.slug);
    assert_eq!(provider, AgentProvider::Codex);
    assert_eq!(CassAgent::from_provider(&provider), Some(CassAgent::Codex));
    assert_eq!(
        CautService::from_provider(&provider),
        Some(CautService::OpenAI)
    );
}

#[test]
fn correlator_process_detection_skips_non_pattern_provider_processes() {
    let mut correlator = AgentCorrelator::new();
    correlator.update_from_pane_info(&pane_with_detection_hints(99, None, Some("cursor")));

    // Cursor is a known provider but not represented in legacy AgentType.
    // Correlator intentionally skips it for metadata consistency.
    assert!(correlator.get_metadata(99).is_none());
    assert!(!correlator.inventory().running.contains_key(&99));
}
