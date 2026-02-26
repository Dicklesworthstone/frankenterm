//! Property-based tests for agent inventory robot API types (ft-dr6zv.2.3).

use proptest::prelude::*;

use frankenterm_core::robot_types::{
    AgentDetectRefreshResult, AgentInventoryData, AgentInventorySummary, InstalledAgentInfo,
    RunningAgentInfo,
};

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn arb_slug() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("claude".to_string()),
        Just("codex".to_string()),
        Just("gemini".to_string()),
        Just("cursor".to_string()),
        Just("cline".to_string()),
        Just("windsurf".to_string()),
        Just("aider".to_string()),
        Just("opencode".to_string()),
        Just("github-copilot".to_string()),
    ]
}

fn arb_state() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("starting".to_string()),
        Just("working".to_string()),
        Just("rate_limited".to_string()),
        Just("waiting_approval".to_string()),
        Just("idle".to_string()),
        Just("active".to_string()),
        Just("unknown".to_string()),
    ]
}

fn arb_source() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("pattern_engine".to_string()),
        Just("pane_title".to_string()),
        Just("process_name".to_string()),
    ]
}

fn arb_installed_agent() -> impl Strategy<Value = InstalledAgentInfo> {
    (
        arb_slug(),
        proptest::option::of("[A-Z][a-z]{2,10}( [A-Z][a-z]{2,10})?"),
        proptest::bool::ANY,
        proptest::collection::vec("[a-z ]{5,30}", 0..3),
        proptest::collection::vec("/[a-z/.]{5,20}", 0..2),
        proptest::option::of("/[a-z/.]{5,20}"),
        proptest::option::of("/[a-z/]{5,15}"),
        proptest::option::of("[0-9]+\\.[0-9]+\\.[0-9]+"),
    )
        .prop_map(
            |(slug, display_name, detected, evidence, root_paths, config, binary, version)| {
                InstalledAgentInfo {
                    slug,
                    display_name,
                    detected,
                    evidence,
                    root_paths,
                    config_path: config,
                    binary_path: binary,
                    version,
                }
            },
        )
}

fn arb_running_agent() -> impl Strategy<Value = (u64, RunningAgentInfo)> {
    (
        1u64..10_000,
        arb_slug(),
        proptest::option::of("[A-Z][a-z]{2,10}"),
        arb_state(),
        proptest::option::of("[a-z0-9-]{5,20}"),
        arb_source(),
    )
        .prop_map(|(pane_id, slug, display_name, state, session_id, source)| {
            (
                pane_id,
                RunningAgentInfo {
                    slug,
                    display_name,
                    state,
                    session_id,
                    source,
                    pane_id,
                },
            )
        })
}

// ---------------------------------------------------------------------------
// AIA-1: InstalledAgentInfo serde roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn aia_1_installed_agent_serde(agent in arb_installed_agent()) {
        let json = serde_json::to_string(&agent).unwrap();
        let parsed: InstalledAgentInfo = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed.slug, agent.slug);
        prop_assert_eq!(parsed.display_name, agent.display_name);
        prop_assert_eq!(parsed.detected, agent.detected);
        prop_assert_eq!(&parsed.evidence, &agent.evidence);
        prop_assert_eq!(&parsed.root_paths, &agent.root_paths);
        prop_assert_eq!(parsed.config_path, agent.config_path);
        prop_assert_eq!(parsed.binary_path, agent.binary_path);
        prop_assert_eq!(parsed.version, agent.version);
    }
}

// ---------------------------------------------------------------------------
// AIA-2: RunningAgentInfo serde roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn aia_2_running_agent_serde((_pane_id, agent) in arb_running_agent()) {
        let json = serde_json::to_string(&agent).unwrap();
        let parsed: RunningAgentInfo = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed.slug, agent.slug);
        prop_assert_eq!(parsed.state, agent.state);
        prop_assert_eq!(parsed.session_id, agent.session_id);
        prop_assert_eq!(parsed.source, agent.source);
        prop_assert_eq!(parsed.pane_id, agent.pane_id);
    }
}

// ---------------------------------------------------------------------------
// AIA-3: AgentInventorySummary serde roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn aia_3_summary_serde(
        installed in 0usize..50,
        running in 0usize..50,
        configured in 0usize..50,
        idle in 0usize..50,
    ) {
        let summary = AgentInventorySummary {
            installed_count: installed,
            running_count: running,
            configured_count: configured,
            installed_but_idle_count: idle,
        };
        let json = serde_json::to_string(&summary).unwrap();
        let parsed: AgentInventorySummary = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed.installed_count, installed);
        prop_assert_eq!(parsed.running_count, running);
        prop_assert_eq!(parsed.configured_count, configured);
        prop_assert_eq!(parsed.installed_but_idle_count, idle);
    }
}

// ---------------------------------------------------------------------------
// AIA-4: AgentInventoryData serde roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn aia_4_inventory_data_serde(
        installed_count in 0usize..5,
        running_count in 0usize..5,
        fs_available in proptest::bool::ANY,
    ) {
        let installed: Vec<InstalledAgentInfo> = (0..installed_count)
            .map(|i| InstalledAgentInfo {
                slug: format!("agent-{}", i),
                display_name: None,
                detected: i % 2 == 0,
                evidence: vec![],
                root_paths: vec![],
                config_path: if i % 3 == 0 { Some("/cfg".to_string()) } else { None },
                binary_path: None,
                version: None,
            })
            .collect();

        let mut running = std::collections::BTreeMap::new();
        for i in 0..running_count {
            let pane_id = (i + 1) as u64;
            running.insert(pane_id, RunningAgentInfo {
                slug: format!("agent-{}", i),
                display_name: None,
                state: "working".to_string(),
                session_id: None,
                source: "pattern_engine".to_string(),
                pane_id,
            });
        }

        let data = AgentInventoryData {
            installed,
            running,
            summary: AgentInventorySummary {
                installed_count,
                running_count,
                configured_count: 0,
                installed_but_idle_count: 0,
            },
            filesystem_detection_available: fs_available,
        };

        let json = serde_json::to_string(&data).unwrap();
        let parsed: AgentInventoryData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed.installed.len(), installed_count);
        prop_assert_eq!(parsed.running.len(), running_count);
        prop_assert_eq!(parsed.filesystem_detection_available, fs_available);
        prop_assert_eq!(parsed.summary.installed_count, installed_count);
        prop_assert_eq!(parsed.summary.running_count, running_count);
    }
}

// ---------------------------------------------------------------------------
// AIA-5: AgentDetectRefreshResult serde roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn aia_5_detect_refresh_serde(
        refreshed in proptest::bool::ANY,
        detected in 0usize..100,
        probed in 0usize..100,
        message in proptest::option::of("[a-z ]{5,30}"),
    ) {
        let result = AgentDetectRefreshResult {
            refreshed,
            detected_count: detected,
            total_probed: probed,
            message,
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: AgentDetectRefreshResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed.refreshed, result.refreshed);
        prop_assert_eq!(parsed.detected_count, result.detected_count);
        prop_assert_eq!(parsed.total_probed, result.total_probed);
        prop_assert_eq!(parsed.message, result.message);
    }
}

// ---------------------------------------------------------------------------
// AIA-6: Empty fields skipped in serialization
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn aia_6_empty_fields_skipped(slug in arb_slug()) {
        let info = InstalledAgentInfo {
            slug,
            display_name: None,
            detected: false,
            evidence: vec![],
            root_paths: vec![],
            config_path: None,
            binary_path: None,
            version: None,
        };
        let json = serde_json::to_string(&info).unwrap();
        // None/empty fields should be omitted
        prop_assert!(!json.contains("display_name"));
        prop_assert!(!json.contains("evidence"));
        prop_assert!(!json.contains("root_paths"));
        prop_assert!(!json.contains("config_path"));
        prop_assert!(!json.contains("binary_path"));
        prop_assert!(!json.contains("version"));
    }
}

// ---------------------------------------------------------------------------
// AIA-7: BTreeMap ordering stability
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn aia_7_running_map_order_stable(count in 1usize..10) {
        let mut running = std::collections::BTreeMap::new();
        for i in 0..count {
            let pane_id = (i * 7 + 3) as u64; // non-sequential IDs
            running.insert(pane_id, RunningAgentInfo {
                slug: "claude".to_string(),
                display_name: None,
                state: "working".to_string(),
                session_id: None,
                source: "pattern_engine".to_string(),
                pane_id,
            });
        }

        let data = AgentInventoryData {
            installed: vec![],
            running,
            summary: AgentInventorySummary::default(),
            filesystem_detection_available: false,
        };

        let json1 = serde_json::to_string(&data).unwrap();
        let json2 = serde_json::to_string(&data).unwrap();
        prop_assert_eq!(json1, json2, "BTreeMap serialization should be deterministic");
    }
}
