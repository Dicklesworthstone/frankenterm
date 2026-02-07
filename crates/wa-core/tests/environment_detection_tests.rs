//! Environment detection integration tests (wa-dug.7)
//!
//! Exercises the full detection → auto-config pipeline deterministically:
//! - PaneInfo fixtures → agent detection → pattern pack selection
//! - PaneInfo fixtures → remote detection → safety policy
//! - ShellInfo → OSC 133 awareness
//! - SystemInfo → poll interval and concurrency scaling
//! - DetectedEnvironment → AutoConfig (end-to-end mapping)
//!
//! All tests are hermetic: no WezTerm, no real filesystem probes, no network.

use std::collections::HashMap;
use std::path::PathBuf;

use chrono::Utc;
use wa_core::environment::{
    AutoConfig, ConnectionType, DetectedAgent, DetectedEnvironment, RemoteHost, ShellInfo,
    SystemInfo, WeztermCapabilities, WeztermInfo,
};
use wa_core::patterns::AgentType;
use wa_core::wezterm::PaneInfo;

// ============================================================================
// Test Helpers
// ============================================================================

fn make_pane(id: u64, title: &str, domain: Option<&str>, cwd: Option<&str>) -> PaneInfo {
    PaneInfo {
        pane_id: id,
        tab_id: 1,
        window_id: 1,
        domain_id: None,
        domain_name: domain.map(String::from),
        workspace: None,
        size: None,
        rows: None,
        cols: None,
        title: Some(title.to_string()),
        cwd: cwd.map(String::from),
        tty_name: None,
        cursor_x: None,
        cursor_y: None,
        cursor_visibility: None,
        left_col: None,
        top_row: None,
        is_active: false,
        is_zoomed: false,
        extra: HashMap::new(),
    }
}

fn make_env(
    cpu_count: usize,
    memory_mb: Option<u64>,
    load_average: Option<f64>,
    agents: Vec<DetectedAgent>,
    remotes: Vec<RemoteHost>,
    shell_type: Option<&str>,
    osc_133: bool,
) -> DetectedEnvironment {
    DetectedEnvironment {
        wezterm: WeztermInfo {
            version: Some("20260101-000000-test".to_string()),
            socket_path: Some(PathBuf::from("/tmp/wezterm-test.sock")),
            is_running: true,
            capabilities: WeztermCapabilities {
                cli_available: true,
                json_output: true,
                multiplexing: true,
                osc_133,
                osc_7: true,
                image_protocol: true,
            },
        },
        shell: ShellInfo {
            shell_path: shell_type.map(|s| format!("/bin/{s}")),
            shell_type: shell_type.map(String::from),
            version: Some("5.2".to_string()),
            config_file: None,
            osc_133_enabled: osc_133,
        },
        agents,
        remotes,
        system: SystemInfo {
            os: "linux".into(),
            arch: "x86_64".into(),
            cpu_count,
            memory_mb,
            load_average,
            detected_at_epoch_ms: 1700000000000,
        },
        detected_at: Utc::now(),
    }
}

// ============================================================================
// Integration: PaneInfo → Agent Detection → AutoConfig Pattern Packs
// ============================================================================

#[test]
fn integration_pane_agents_to_pattern_packs() {
    // Given panes with agent titles
    let panes = vec![
        make_pane(1, "codex @ /project", Some("local"), None),
        make_pane(2, "Claude Code - workspace", Some("local"), None),
        make_pane(3, "Gemini Advanced", Some("local"), None),
        make_pane(4, "vim", Some("local"), None),
    ];

    // When we detect agents from panes
    let agents = wa_core::environment::detect_agents_from_panes(&panes);
    assert_eq!(agents.len(), 3);

    // And build auto-config from detected environment
    let env = make_env(8, Some(16384), Some(0.5), agents, vec![], Some("zsh"), true);
    let auto = AutoConfig::from_environment(&env);

    // Then all three agent packs are selected plus core
    assert!(auto.pattern_packs.contains(&"builtin:core".to_string()));
    assert!(auto.pattern_packs.contains(&"builtin:codex".to_string()));
    assert!(
        auto.pattern_packs
            .contains(&"builtin:claude_code".to_string())
    );
    assert!(auto.pattern_packs.contains(&"builtin:gemini".to_string()));
    assert_eq!(auto.pattern_packs.len(), 4);
}

#[test]
fn integration_no_agent_panes_gives_core_only() {
    let panes = vec![
        make_pane(1, "zsh", Some("local"), None),
        make_pane(2, "htop", Some("local"), None),
    ];

    let agents = wa_core::environment::detect_agents_from_panes(&panes);
    assert!(agents.is_empty());

    let env = make_env(4, Some(8192), None, agents, vec![], Some("zsh"), false);
    let auto = AutoConfig::from_environment(&env);
    assert_eq!(auto.pattern_packs, vec!["builtin:core"]);
}

// ============================================================================
// Integration: PaneInfo → Remote Detection → Safety Policy
// ============================================================================

#[test]
fn integration_ssh_remotes_enable_strict_safety() {
    let panes = vec![
        make_pane(1, "bash", Some("ssh:dev-server"), None),
        make_pane(2, "bash", Some("ssh:dev-server"), None),
        make_pane(3, "vim", Some("local"), None),
    ];

    let remotes = wa_core::environment::detect_remotes_from_panes(&panes);
    assert_eq!(remotes.len(), 1);
    assert_eq!(remotes[0].connection_type, ConnectionType::Ssh);
    assert_eq!(remotes[0].pane_ids.len(), 2);

    let env = make_env(
        8,
        Some(16384),
        Some(0.5),
        vec![],
        remotes,
        Some("zsh"),
        true,
    );
    let auto = AutoConfig::from_environment(&env);
    assert!(auto.strict_safety);
    assert!(auto.rate_limit_per_pane < 30);
    assert!(auto.poll_interval_ms >= 200);
}

#[test]
fn integration_production_remotes_max_caution() {
    let panes = vec![
        make_pane(1, "bash", Some("ssh:web-production-01"), None),
        make_pane(2, "bash", Some("local"), None),
    ];

    let remotes = wa_core::environment::detect_remotes_from_panes(&panes);
    assert_eq!(remotes.len(), 1);

    let env = make_env(
        8,
        Some(16384),
        Some(0.5),
        vec![],
        remotes,
        Some("bash"),
        false,
    );
    let auto = AutoConfig::from_environment(&env);
    assert!(auto.strict_safety);
    assert!(auto.rate_limit_per_pane <= 10);
}

#[test]
fn integration_mixed_remote_types_detected() {
    let panes = vec![
        make_pane(1, "bash", Some("ssh:server1"), None),
        make_pane(2, "bash", Some("wsl:Ubuntu"), None),
        make_pane(3, "bash", Some("docker:webapp"), None),
        make_pane(4, "zsh", Some("local"), None),
    ];

    let remotes = wa_core::environment::detect_remotes_from_panes(&panes);
    assert_eq!(remotes.len(), 3);

    let types: Vec<ConnectionType> = remotes.iter().map(|r| r.connection_type).collect();
    assert!(types.contains(&ConnectionType::Ssh));
    assert!(types.contains(&ConnectionType::Wsl));
    assert!(types.contains(&ConnectionType::Docker));
}

// ============================================================================
// Integration: Remote detection via CWD URI (no domain_name)
// ============================================================================

#[test]
fn integration_remote_inferred_from_cwd_uri() {
    let panes = vec![make_pane(
        1,
        "bash",
        None,
        Some("file://remote-host/home/user"),
    )];

    let remotes = wa_core::environment::detect_remotes_from_panes(&panes);
    assert_eq!(remotes.len(), 1);
    assert_eq!(remotes[0].connection_type, ConnectionType::Ssh);
    assert!(
        remotes[0].hostname.contains("remote-host"),
        "hostname should contain 'remote-host', got: {}",
        remotes[0].hostname
    );
}

// ============================================================================
// Integration: Shell Detection → OSC 133 awareness
// ============================================================================

#[test]
fn integration_shell_detection_paths() {
    // Bash
    let bash = ShellInfo::from_shell_path(Some("/bin/bash"));
    assert_eq!(bash.shell_type.as_deref(), Some("bash"));

    // Zsh
    let zsh = ShellInfo::from_shell_path(Some("/usr/bin/zsh"));
    assert_eq!(zsh.shell_type.as_deref(), Some("zsh"));

    // Fish
    let fish = ShellInfo::from_shell_path(Some("/usr/local/bin/fish"));
    assert_eq!(fish.shell_type.as_deref(), Some("fish"));

    // Unknown shell
    let nu = ShellInfo::from_shell_path(Some("/opt/bin/nu"));
    assert!(nu.shell_type.is_none());

    // Missing shell
    let none = ShellInfo::from_shell_path(None);
    assert!(none.shell_type.is_none());
    assert!(!none.osc_133_enabled);
}

// ============================================================================
// Integration: SystemInfo → AutoConfig scaling
// ============================================================================

#[test]
fn integration_system_scaling_low_resource() {
    // 2 cores, 1GB RAM, load 5.0 → constrained system
    let env = make_env(
        2,
        Some(1024),
        Some(5.0),
        vec![],
        vec![],
        Some("bash"),
        false,
    );
    let auto = AutoConfig::from_environment(&env);

    // Low resources → conservative settings
    assert!(auto.poll_interval_ms >= 300, "low memory → slow polling");
    assert_eq!(auto.max_concurrent_captures, 4, "low CPU → min captures");
    assert_eq!(auto.min_poll_interval_ms, 100, "2 CPUs → 100ms min");
}

#[test]
fn integration_system_scaling_high_resource() {
    // 16 cores, 64GB RAM, idle
    let env = make_env(
        16,
        Some(65536),
        Some(0.1),
        vec![],
        vec![],
        Some("zsh"),
        true,
    );
    let auto = AutoConfig::from_environment(&env);

    // Abundant resources → aggressive settings
    assert_eq!(auto.poll_interval_ms, 100, "idle system → fast polling");
    assert_eq!(
        auto.max_concurrent_captures, 32,
        "many cores → max captures"
    );
    assert_eq!(auto.min_poll_interval_ms, 25, "8+ CPUs → 25ms min");
}

// ============================================================================
// Integration: Full DetectedEnvironment → AutoConfig (end-to-end)
// ============================================================================

#[test]
fn integration_full_detection_to_autoconfig() {
    // Realistic scenario: codex agent on SSH, 8 cores, 16GB, moderate load
    let panes = vec![
        make_pane(1, "codex @ /project", Some("local"), None),
        make_pane(2, "bash", Some("ssh:staging-01"), None),
        make_pane(3, "zsh", Some("local"), None),
    ];

    let agents = wa_core::environment::detect_agents_from_panes(&panes);
    let remotes = wa_core::environment::detect_remotes_from_panes(&panes);

    let env = make_env(
        8,
        Some(16384),
        Some(4.0), // per-core 0.5 → idle
        agents,
        remotes,
        Some("zsh"),
        true,
    );
    let auto = AutoConfig::from_environment(&env);

    // Agent detected → codex pack enabled
    assert!(auto.pattern_packs.contains(&"builtin:codex".to_string()));

    // Remote detected → strict + rate limited
    assert!(auto.strict_safety);
    assert!(auto.rate_limit_per_pane < 30);

    // Remote → interval >= 200
    assert!(auto.poll_interval_ms >= 200);

    // System is beefy
    assert_eq!(auto.max_concurrent_captures, 16);
    assert_eq!(auto.min_poll_interval_ms, 25);

    // Recommendations generated
    assert!(!auto.recommendations.is_empty());
    let rec_keys: Vec<&str> = auto
        .recommendations
        .iter()
        .map(|r| r.key.as_str())
        .collect();
    assert!(rec_keys.contains(&"patterns.packs"));
    assert!(rec_keys.contains(&"safety.rate_limit_per_pane"));
}

// ============================================================================
// Serde: Full DetectedEnvironment JSON round-trip
// ============================================================================

#[test]
fn integration_detected_environment_json_stable() {
    let agents = vec![DetectedAgent {
        agent_type: AgentType::Codex,
        pane_id: 1,
        confidence: 0.9,
        indicators: vec!["title:codex".into()],
    }];
    let remotes = vec![RemoteHost {
        hostname: "server1".into(),
        connection_type: ConnectionType::Ssh,
        pane_ids: vec![1, 2],
    }];

    let env = make_env(4, Some(8192), Some(1.5), agents, remotes, Some("zsh"), true);
    let json = serde_json::to_string_pretty(&env).unwrap();

    // Required top-level fields
    assert!(json.contains("\"wezterm\""));
    assert!(json.contains("\"shell\""));
    assert!(json.contains("\"agents\""));
    assert!(json.contains("\"remotes\""));
    assert!(json.contains("\"system\""));
    assert!(json.contains("\"detected_at\""));

    // Nested fields
    assert!(json.contains("\"cli_available\""));
    assert!(json.contains("\"osc_133\""));
    assert!(json.contains("\"shell_type\""));
    assert!(json.contains("\"agent_type\""));
    assert!(json.contains("\"connection_type\""));
    assert!(json.contains("\"cpu_count\""));
    assert!(json.contains("\"memory_mb\""));
}

#[test]
fn integration_autoconfig_json_stable() {
    let agents = vec![DetectedAgent {
        agent_type: AgentType::ClaudeCode,
        pane_id: 2,
        confidence: 0.8,
        indicators: vec!["title:claude".into()],
    }];
    let remotes = vec![RemoteHost {
        hostname: "prod-01".into(),
        connection_type: ConnectionType::Ssh,
        pane_ids: vec![3],
    }];

    let env = make_env(
        4,
        Some(8192),
        Some(6.0),
        agents,
        remotes,
        Some("bash"),
        false,
    );
    let auto = AutoConfig::from_environment(&env);
    let json = serde_json::to_string_pretty(&auto).unwrap();

    // Required top-level fields
    assert!(json.contains("\"poll_interval_ms\""));
    assert!(json.contains("\"min_poll_interval_ms\""));
    assert!(json.contains("\"max_concurrent_captures\""));
    assert!(json.contains("\"pattern_packs\""));
    assert!(json.contains("\"strict_safety\""));
    assert!(json.contains("\"rate_limit_per_pane\""));
    assert!(json.contains("\"recommendations\""));

    // Recommendations have all fields
    for rec in &auto.recommendations {
        let rec_json = serde_json::to_string(rec).unwrap();
        assert!(rec_json.contains("\"key\""));
        assert!(rec_json.contains("\"value\""));
        assert!(rec_json.contains("\"reason\""));
        assert!(rec_json.contains("\"source\""));
    }
}

// ============================================================================
// Determinism: Same input → same auto-config output
// ============================================================================

#[test]
fn integration_autoconfig_deterministic() {
    let agents = vec![DetectedAgent {
        agent_type: AgentType::Gemini,
        pane_id: 5,
        confidence: 0.7,
        indicators: vec!["title:gemini".into()],
    }];
    let remotes = vec![RemoteHost {
        hostname: "dev-01".into(),
        connection_type: ConnectionType::Ssh,
        pane_ids: vec![6],
    }];

    let env1 = make_env(
        8,
        Some(16384),
        Some(2.5),
        agents.clone(),
        remotes.clone(),
        Some("fish"),
        true,
    );
    let env2 = make_env(
        8,
        Some(16384),
        Some(2.5),
        agents,
        remotes,
        Some("fish"),
        true,
    );

    let auto1 = AutoConfig::from_environment(&env1);
    let auto2 = AutoConfig::from_environment(&env2);

    assert_eq!(auto1.poll_interval_ms, auto2.poll_interval_ms);
    assert_eq!(auto1.min_poll_interval_ms, auto2.min_poll_interval_ms);
    assert_eq!(auto1.max_concurrent_captures, auto2.max_concurrent_captures);
    assert_eq!(auto1.pattern_packs, auto2.pattern_packs);
    assert_eq!(auto1.strict_safety, auto2.strict_safety);
    assert_eq!(auto1.rate_limit_per_pane, auto2.rate_limit_per_pane);
    assert_eq!(auto1.recommendations.len(), auto2.recommendations.len());
}

// ============================================================================
// Edge case: All detections return None/empty
// ============================================================================

#[test]
fn integration_all_empty_detections_safe() {
    let env = DetectedEnvironment {
        wezterm: WeztermInfo {
            version: None,
            socket_path: None,
            is_running: false,
            capabilities: WeztermCapabilities::default(),
        },
        shell: ShellInfo {
            shell_path: None,
            shell_type: None,
            version: None,
            config_file: None,
            osc_133_enabled: false,
        },
        agents: vec![],
        remotes: vec![],
        system: SystemInfo {
            os: "unknown".into(),
            arch: "unknown".into(),
            cpu_count: 1,
            memory_mb: None,
            load_average: None,
            detected_at_epoch_ms: 0,
        },
        detected_at: Utc::now(),
    };

    let auto = AutoConfig::from_environment(&env);

    // Safe defaults
    assert_eq!(auto.poll_interval_ms, 100);
    assert!(!auto.strict_safety);
    assert_eq!(auto.rate_limit_per_pane, 30);
    assert_eq!(auto.pattern_packs, vec!["builtin:core"]);
    assert_eq!(auto.max_concurrent_captures, 4); // 1 CPU × 2 = 2, floor 4

    // Serialization still works
    let json = serde_json::to_string(&auto).unwrap();
    assert!(!json.is_empty());
    let env_json = serde_json::to_string(&env).unwrap();
    assert!(!env_json.is_empty());
}
