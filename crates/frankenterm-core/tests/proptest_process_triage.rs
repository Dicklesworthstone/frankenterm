//! Property-based tests for process triage invariants.
//!
//! Bead: wa-mkdn
//!
//! Validates:
//! 1. TriageCategory: ordering matches priority values (1..9)
//! 2. TriageCategory: priority() returns discriminant
//! 3. TriageCategory: is_auto_safe only for priorities 1-4
//! 4. TriageCategory: is_protected only for priorities 8-9
//! 5. TriageCategory: auto_safe and protected are mutually exclusive
//! 6. TriageCategory: Display is non-empty snake_case
//! 7. TriageCategory: serde roundtrip
//! 8. TriageAction: Display is non-empty
//! 9. TriageAction: serde roundtrip (ReapZombie)
//! 10. TriageAction: serde roundtrip (GracefulKill)
//! 11. TriageAction: serde roundtrip (ForceKill/Renice/Protect)
//! 12. TriageAction: serde roundtrip (FlagForReview)
//! 13. ClassifiedProcess: serde roundtrip
//! 14. TriageConfig: serde roundtrip
//! 15. TriageConfig::default: sensible values
//! 16. classify: system processes always SystemProcess
//! 17. classify: zombies always Zombie (unless system process)
//! 18. classify: stuck test runners → StuckTest
//! 19. classify: stuck CLI tools → StuckCli
//! 20. classify: build tools → DuplicateBuild
//! 21. classify: abandoned dev servers → AbandonedServer
//! 22. classify: active agents → ActiveAgent (protected)
//! 23. classify: confused agents → ConfusedAgent
//! 24. TriagePlan: auto_safe_entries subset of is_auto_safe
//! 25. TriagePlan: entries sorted by category

use std::time::Duration;

use proptest::prelude::*;

use frankenterm_core::process_tree::{ProcessNode, ProcessState, ProcessTree};
use frankenterm_core::process_triage::{
    ClassifiedProcess, ProcessContext, TriageAction, TriageCategory, TriageConfig, build_plan,
    classify,
};

// =============================================================================
// Strategies
// =============================================================================

fn arb_triage_category() -> impl Strategy<Value = TriageCategory> {
    prop_oneof![
        Just(TriageCategory::Zombie),
        Just(TriageCategory::StuckTest),
        Just(TriageCategory::StuckCli),
        Just(TriageCategory::DuplicateBuild),
        Just(TriageCategory::AbandonedServer),
        Just(TriageCategory::StaleSession),
        Just(TriageCategory::ConfusedAgent),
        Just(TriageCategory::ActiveAgent),
        Just(TriageCategory::SystemProcess),
    ]
}

fn arb_triage_action() -> impl Strategy<Value = TriageAction> {
    prop_oneof![
        (1_u32..100000).prop_map(|pid| TriageAction::ReapZombie { parent_pid: pid }),
        (1_u64..3600).prop_map(|s| TriageAction::GracefulKill {
            grace_period: Duration::from_secs(s)
        }),
        Just(TriageAction::ForceKill),
        Just(TriageAction::Renice),
        Just(TriageAction::Protect),
        "[a-zA-Z0-9 ]{1,40}".prop_map(|r| TriageAction::FlagForReview { reason: r }),
    ]
}

fn make_node(pid: u32, parent_pid: u32, name: &str, state: ProcessState) -> ProcessNode {
    ProcessNode {
        pid,
        ppid: parent_pid,
        name: name.into(),
        argv: vec![],
        state,
        rss_kb: 10000,
        children: vec![],
    }
}

fn make_context(age_hours: f64, cpu: f64, is_test: bool) -> ProcessContext {
    ProcessContext {
        age: Duration::from_secs_f64(age_hours * 3600.0),
        cpu_percent: cpu,
        is_test,
    }
}

// System process names (from the source)
const SYSTEM_NAMES: &[&str] = &[
    "init",
    "systemd",
    "launchd",
    "kernel_task",
    "WindowServer",
    "sshd",
    "postgres",
    "mysqld",
    "dockerd",
    "containerd",
    "kubelet",
    "cron",
    "rsyslogd",
    "loginwindow",
    "CoreServicesUIAgent",
    "mds",
    "mds_stores",
    "mdworker",
    "wezterm-mux-server",
];

const TEST_RUNNER_NAMES: &[&str] = &["cargo-nextest", "pytest", "jest", "mocha", "vitest"];

const CLI_TOOL_NAMES: &[&str] = &[
    "git", "gh", "npm", "npx", "yarn", "pnpm", "bun", "vercel", "pip", "pip3",
];

const BUILD_TOOL_NAMES: &[&str] = &[
    "cargo", "rustc", "cc1", "gcc", "g++", "clang", "make", "cmake", "ninja",
];

// "django" excluded: contains "go" substring → matches test runner check first.
// "bun" excluded: also appears in CLI_TOOLS, gets classified as StuckCli if old enough.
const DEV_SERVER_NAMES: &[&str] = &["next", "vite", "uvicorn", "gunicorn", "flask"];

const AGENT_NAMES: &[&str] = &[
    "claude",
    "claude-code",
    "codex",
    "gemini",
    "aider",
    "copilot",
];

// =============================================================================
// Property 1: TriageCategory ordering matches priority values
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn category_ordering_matches_priority(
        a in arb_triage_category(),
        b in arb_triage_category(),
    ) {
        match a.priority().cmp(&b.priority()) {
            std::cmp::Ordering::Less => prop_assert!(a < b, "{:?} (p={}) should be < {:?} (p={})", a, a.priority(), b, b.priority()),
            std::cmp::Ordering::Greater => prop_assert!(a > b, "{:?} (p={}) should be > {:?} (p={})", a, a.priority(), b, b.priority()),
            std::cmp::Ordering::Equal => prop_assert_eq!(a, b),
        }
    }
}

// =============================================================================
// Property 2: priority() returns discriminant (1..=9)
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn category_priority_range(
        cat in arb_triage_category(),
    ) {
        let p = cat.priority();
        prop_assert!((1..=9).contains(&p),
            "{:?} priority {} not in [1,9]", cat, p);
    }
}

// =============================================================================
// Property 3: is_auto_safe only for priorities 1-4
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn auto_safe_only_low_priority(
        cat in arb_triage_category(),
    ) {
        if cat.is_auto_safe() {
            prop_assert!(cat.priority() <= 4,
                "{:?} is auto_safe but priority {} > 4", cat, cat.priority());
        }
    }
}

// =============================================================================
// Property 4: is_protected only for priorities 8-9
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn protected_only_high_priority(
        cat in arb_triage_category(),
    ) {
        if cat.is_protected() {
            prop_assert!(cat.priority() >= 8,
                "{:?} is protected but priority {} < 8", cat, cat.priority());
        }
    }
}

// =============================================================================
// Property 5: auto_safe and protected are mutually exclusive
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn auto_safe_protected_exclusive(
        cat in arb_triage_category(),
    ) {
        prop_assert!(!(cat.is_auto_safe() && cat.is_protected()),
            "{:?} should not be both auto_safe and protected", cat);
    }
}

// =============================================================================
// Property 6: Display is non-empty snake_case
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn category_display_nonempty_snake(
        cat in arb_triage_category(),
    ) {
        let s = cat.to_string();
        prop_assert!(!s.is_empty());
        prop_assert!(s.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "Display '{}' should be snake_case", s);
    }
}

// =============================================================================
// Property 7: TriageCategory serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn category_serde_roundtrip(
        cat in arb_triage_category(),
    ) {
        let json = serde_json::to_string(&cat).unwrap();
        let back: TriageCategory = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, cat);
    }
}

// =============================================================================
// Property 8: TriageAction Display is non-empty
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn action_display_nonempty(
        action in arb_triage_action(),
    ) {
        let s = action.to_string();
        prop_assert!(!s.is_empty(), "TriageAction Display should not be empty");
    }
}

// =============================================================================
// Property 9-12: TriageAction serde roundtrips
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn action_reap_zombie_serde(
        parent_pid in 1_u32..100000,
    ) {
        let action = TriageAction::ReapZombie { parent_pid };
        let json = serde_json::to_string(&action).unwrap();
        let back: TriageAction = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, action);
    }

    #[test]
    fn action_graceful_kill_serde(
        secs in 1_u64..3600,
    ) {
        let action = TriageAction::GracefulKill { grace_period: Duration::from_secs(secs) };
        let json = serde_json::to_string(&action).unwrap();
        let back: TriageAction = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, action);
    }

    #[test]
    fn action_simple_serde(
        variant in prop_oneof![
            Just(TriageAction::ForceKill),
            Just(TriageAction::Renice),
            Just(TriageAction::Protect),
        ],
    ) {
        let json = serde_json::to_string(&variant).unwrap();
        let back: TriageAction = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, variant);
    }

    #[test]
    fn action_flag_for_review_serde(
        reason in "[a-zA-Z0-9 ]{1,40}",
    ) {
        let action = TriageAction::FlagForReview { reason };
        let json = serde_json::to_string(&action).unwrap();
        let back: TriageAction = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, action);
    }
}

// =============================================================================
// Property 13: ClassifiedProcess serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn classified_process_serde(
        pid in 1_u32..100000,
        name in "[a-z]{3,15}",
        cat in arb_triage_category(),
        action in arb_triage_action(),
        reason in "[a-zA-Z0-9 ]{5,40}",
        pane_id in proptest::option::of(1_u64..1000),
    ) {
        let cp = ClassifiedProcess {
            pid,
            name: name.clone(),
            category: cat,
            action: action.clone(),
            reason: reason.clone(),
            pane_id,
        };
        let json = serde_json::to_string(&cp).unwrap();
        let back: ClassifiedProcess = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.pid, pid);
        prop_assert_eq!(back.name, name);
        prop_assert_eq!(back.category, cat);
        prop_assert_eq!(back.action, action);
        prop_assert_eq!(back.reason, reason);
        prop_assert_eq!(back.pane_id, pane_id);
    }
}

// =============================================================================
// Property 14: TriageConfig serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn config_serde_roundtrip(
        stuck_test in 1.0_f64..48.0,
        stuck_cli in 1.0_f64..120.0,
        agent_confused in 4.0_f64..72.0,
        server_abandoned in 6.0_f64..168.0,
        auto_safe in proptest::bool::ANY,
    ) {
        let config = TriageConfig {
            enabled: true,
            auto_safe,
            stuck_test_hours: stuck_test,
            stuck_cli_minutes: stuck_cli,
            agent_confused_hours: agent_confused,
            server_abandoned_hours: server_abandoned,
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: TriageConfig = serde_json::from_str(&json).unwrap();
        prop_assert!((back.stuck_test_hours - config.stuck_test_hours).abs() < 1e-10);
        prop_assert!((back.stuck_cli_minutes - config.stuck_cli_minutes).abs() < 1e-10);
        prop_assert!((back.agent_confused_hours - config.agent_confused_hours).abs() < 1e-10);
        prop_assert!((back.server_abandoned_hours - config.server_abandoned_hours).abs() < 1e-10);
        prop_assert_eq!(back.auto_safe, config.auto_safe);
    }
}

// =============================================================================
// Property 15: TriageConfig::default sensible values
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10))]

    #[test]
    fn config_defaults_sensible(_dummy in 0..1_u32) {
        let config = TriageConfig::default();
        prop_assert!(config.enabled);
        prop_assert!(config.auto_safe);
        prop_assert!(config.stuck_test_hours > 0.0);
        prop_assert!(config.stuck_cli_minutes > 0.0);
        prop_assert!(config.agent_confused_hours > 0.0);
        prop_assert!(config.server_abandoned_hours > 0.0);
        // Thresholds should increase: CLI < test < agent < server
        prop_assert!(config.stuck_cli_minutes / 60.0 < config.stuck_test_hours,
            "CLI threshold should be shorter than test threshold");
        prop_assert!(config.stuck_test_hours < config.agent_confused_hours,
            "test threshold should be shorter than agent threshold");
    }
}

// =============================================================================
// Property 16: system processes always classified as SystemProcess
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn system_processes_always_system(
        idx in 0..SYSTEM_NAMES.len(),
        age_hours in 0.0_f64..200.0,
        cpu in 0.0_f64..100.0,
    ) {
        let name = SYSTEM_NAMES[idx];
        let node = make_node(1, 0, name, ProcessState::Running);
        let ctx = make_context(age_hours, cpu, false);
        let (cat, action, _) = classify(&node, &ctx);
        prop_assert_eq!(cat, TriageCategory::SystemProcess,
            "system process '{}' should be SystemProcess", name);
        prop_assert!(matches!(action, TriageAction::Protect),
            "system process '{}' should have Protect action", name);
    }
}

// =============================================================================
// Property 17: zombie processes always classified as Zombie (unless system)
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn zombies_always_zombie(
        pid in 2_u32..100000,
        ppid in 1_u32..100000,
        name in "[a-z]{3,10}",
        age_hours in 0.0_f64..200.0,
        cpu in 0.0_f64..100.0,
    ) {
        // Skip system process names (they take precedence)
        prop_assume!(!SYSTEM_NAMES.iter().any(|s| s.eq_ignore_ascii_case(&name)));
        let node = make_node(pid, ppid, &name, ProcessState::Zombie);
        let ctx = make_context(age_hours, cpu, false);
        let (cat, _, _) = classify(&node, &ctx);
        prop_assert_eq!(cat, TriageCategory::Zombie,
            "zombie '{}' should be Zombie category", name);
    }
}

// =============================================================================
// Property 18: stuck test runners → StuckTest
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn stuck_test_runners_classified(
        idx in 0..TEST_RUNNER_NAMES.len(),
        extra_hours in 0.1_f64..50.0,
        cpu in 0.0_f64..0.99,
    ) {
        let name = TEST_RUNNER_NAMES[idx];
        let age_hours = 12.0 + extra_hours; // Always > 12h threshold
        let node = make_node(200, 100, name, ProcessState::Sleeping);
        let ctx = make_context(age_hours, cpu, false);
        let (cat, _, _) = classify(&node, &ctx);
        prop_assert_eq!(cat, TriageCategory::StuckTest,
            "test runner '{}' at {:.1}h/{:.1}% should be StuckTest", name, age_hours, cpu);
    }
}

// =============================================================================
// Property 19: stuck CLI tools → StuckCli
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn stuck_cli_tools_classified(
        idx in 0..CLI_TOOL_NAMES.len(),
        extra_minutes in 1.0_f64..120.0,
    ) {
        let name = CLI_TOOL_NAMES[idx];
        // Skip "bun" since it also matches DEV_SERVERS and BUILD_TOOLS processing
        // The classify function checks test runner / CLI tool / build tool in order
        prop_assume!(name != "bun");
        let age_hours = (5.0 + extra_minutes) / 60.0; // Always > 5min threshold
        let node = make_node(300, 100, name, ProcessState::Sleeping);
        let ctx = make_context(age_hours, 0.0, false);
        let (cat, _, _) = classify(&node, &ctx);
        prop_assert_eq!(cat, TriageCategory::StuckCli,
            "CLI tool '{}' at {:.1}min should be StuckCli", name, age_hours * 60.0);
    }
}

// =============================================================================
// Property 20: build tools → DuplicateBuild
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn build_tools_classified(
        idx in 0..BUILD_TOOL_NAMES.len(),
        age_hours in 0.0_f64..1.0,
        cpu in 10.0_f64..100.0,
    ) {
        let name = BUILD_TOOL_NAMES[idx];
        let node = make_node(400, 100, name, ProcessState::Running);
        let ctx = make_context(age_hours, cpu, false);
        let (cat, _, _) = classify(&node, &ctx);
        prop_assert_eq!(cat, TriageCategory::DuplicateBuild,
            "build tool '{}' should be DuplicateBuild", name);
    }
}

// =============================================================================
// Property 21: abandoned dev servers → AbandonedServer
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn abandoned_dev_servers_classified(
        idx in 0..DEV_SERVER_NAMES.len(),
        extra_hours in 0.1_f64..72.0,
    ) {
        let name = DEV_SERVER_NAMES[idx];
        let age_hours = 24.0 + extra_hours; // Always > 24h threshold
        let node = make_node(500, 100, name, ProcessState::Sleeping);
        let ctx = make_context(age_hours, 0.0, false);
        let (cat, _, _) = classify(&node, &ctx);
        prop_assert_eq!(cat, TriageCategory::AbandonedServer,
            "dev server '{}' at {:.0}h should be AbandonedServer", name, age_hours);
    }
}

// =============================================================================
// Property 22: active agents → ActiveAgent (protected)
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn active_agents_protected(
        idx in 0..AGENT_NAMES.len(),
        age_hours in 0.1_f64..15.9,
        cpu in 1.0_f64..100.0,
    ) {
        let name = AGENT_NAMES[idx];
        let node = make_node(700, 100, name, ProcessState::Running);
        let ctx = make_context(age_hours, cpu, false);
        let (cat, action, _) = classify(&node, &ctx);
        prop_assert_eq!(cat, TriageCategory::ActiveAgent,
            "agent '{}' at {:.1}h should be ActiveAgent", name, age_hours);
        prop_assert!(matches!(action, TriageAction::Protect),
            "active agent '{}' should be protected", name);
    }
}

// =============================================================================
// Property 23: confused agents → ConfusedAgent
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn confused_agents_classified(
        idx in 0..AGENT_NAMES.len(),
        extra_hours in 0.1_f64..50.0,
    ) {
        let name = AGENT_NAMES[idx];
        let age_hours = 16.0 + extra_hours; // Always > 16h threshold
        let node = make_node(700, 100, name, ProcessState::Sleeping);
        let ctx = make_context(age_hours, 2.0, false);
        let (cat, _, _) = classify(&node, &ctx);
        prop_assert_eq!(cat, TriageCategory::ConfusedAgent,
            "agent '{}' at {:.0}h should be ConfusedAgent", name, age_hours);
    }
}

// =============================================================================
// Property 24: TriagePlan auto_safe_entries are all auto_safe
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn plan_auto_safe_entries_valid(
        n_procs in 1_usize..10,
    ) {
        // Build a mixed tree with zombie + agent processes
        let mut children = Vec::new();
        for i in 0..n_procs as u32 {
            if i % 2 == 0 {
                children.push(make_node(i + 2, 1, "defunct", ProcessState::Zombie));
            } else {
                children.push(make_node(i + 2, 1, "claude", ProcessState::Running));
            }
        }
        let root = ProcessNode {
            pid: 1,
            ppid: 0,
            name: "bash".into(),
            argv: vec![],
            state: ProcessState::Sleeping,
            rss_kb: 5000,
            children,
        };
        let tree = ProcessTree {
            root,
            total_processes: n_procs + 1,
            total_rss_kb: 50000,
        };
        let context_fn = |_pid: u32| make_context(2.0, 10.0, false);
        let plan = build_plan(&[(tree, Some(1), &context_fn)]);

        for entry in plan.auto_safe_entries() {
            prop_assert!(entry.category.is_auto_safe(),
                "auto_safe_entries should only contain auto_safe categories, got {:?}", entry.category);
        }
    }
}

// =============================================================================
// Property 25: TriagePlan entries sorted by category
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn plan_entries_sorted(
        n_procs in 1_usize..8,
    ) {
        let mut children = Vec::new();
        let names = ["defunct", "claude", "git", "rustc", "next"];
        let states = [ProcessState::Zombie, ProcessState::Running, ProcessState::Sleeping,
                      ProcessState::Running, ProcessState::Sleeping];
        for i in 0..n_procs.min(names.len()) as u32 {
            children.push(make_node(i + 2, 1, names[i as usize], states[i as usize]));
        }
        let root = ProcessNode {
            pid: 1,
            ppid: 0,
            name: "bash".into(),
            argv: vec![],
            state: ProcessState::Sleeping,
            rss_kb: 5000,
            children,
        };
        let tree = ProcessTree {
            root,
            total_processes: n_procs + 1,
            total_rss_kb: 50000,
        };
        let context_fn = |_pid: u32| make_context(25.0, 0.5, false);
        let plan = build_plan(&[(tree, None, &context_fn)]);

        for w in plan.entries.windows(2) {
            prop_assert!(w[0].category <= w[1].category,
                "entries should be sorted: {:?} <= {:?}", w[0].category, w[1].category);
        }
    }
}
