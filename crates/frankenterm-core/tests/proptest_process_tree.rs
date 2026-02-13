//! Property-based tests for `process_tree` module.
//!
//! Covers:
//! - Serde roundtrip for all types (ProcessTree, ProcessNode, ProcessState,
//!   PaneActivity, ProcessTreeConfig)
//! - Activity inference priority: Agent > Compile > Test > VCS > Edit > Active > Idle
//! - exe_names(): sorted, deduplicated, complete
//! - contains_process(): consistent with exe_names()
//! - subtree_rss_kb(): equals sum of all nodes in subtree
//! - count_tree consistency: total_processes and total_rss_kb match tree walk
//! - Display implementations: non-empty, lowercase

use frankenterm_core::process_tree::{
    PaneActivity, ProcessNode, ProcessState, ProcessTree, ProcessTreeConfig, infer_activity,
};
use proptest::prelude::*;

// =============================================================================
// Strategies
// =============================================================================

fn arb_process_state() -> impl Strategy<Value = ProcessState> {
    prop_oneof![
        Just(ProcessState::Running),
        Just(ProcessState::Sleeping),
        Just(ProcessState::DiskSleep),
        Just(ProcessState::Stopped),
        Just(ProcessState::Zombie),
        Just(ProcessState::Unknown),
    ]
}

fn arb_pane_activity() -> impl Strategy<Value = PaneActivity> {
    prop_oneof![
        Just(PaneActivity::Idle),
        Just(PaneActivity::Compiling),
        Just(PaneActivity::Testing),
        Just(PaneActivity::VersionControl),
        Just(PaneActivity::AgentRunning),
        Just(PaneActivity::Editing),
        Just(PaneActivity::Active),
    ]
}

/// Generate an arbitrary process name. Mix of known tool names and random names.
fn arb_process_name() -> impl Strategy<Value = String> {
    prop_oneof![
        // Agent CLIs
        Just("claude".to_string()),
        Just("claude-code".to_string()),
        Just("codex".to_string()),
        Just("gemini".to_string()),
        Just("aider".to_string()),
        Just("copilot".to_string()),
        // Compilation
        Just("cargo".to_string()),
        Just("rustc".to_string()),
        Just("gcc".to_string()),
        Just("make".to_string()),
        Just("webpack".to_string()),
        // Testing
        Just("pytest".to_string()),
        Just("jest".to_string()),
        Just("cargo-nextest".to_string()),
        // VCS
        Just("git".to_string()),
        Just("hg".to_string()),
        Just("gh".to_string()),
        // Editors
        Just("vim".to_string()),
        Just("nvim".to_string()),
        Just("emacs".to_string()),
        Just("code".to_string()),
        Just("rust-analyzer".to_string()),
        // Generic
        Just("bash".to_string()),
        Just("zsh".to_string()),
        Just("fish".to_string()),
        Just("node".to_string()),
        Just("python".to_string()),
        Just("my-custom-tool".to_string()),
        "[a-z][a-z0-9_-]{0,15}".prop_map(|s| s),
    ]
}

/// Generate a leaf process node (no children).
fn arb_leaf_node() -> impl Strategy<Value = ProcessNode> {
    (
        1..100_000u32,                                                        // pid
        0..100_000u32,                                                        // ppid
        arb_process_name(),                                                   // name
        arb_process_state(),                                                  // state
        0..500_000u64,                                                        // rss_kb
        proptest::collection::vec("[a-z0-9_.-]{1,10}".prop_map(|s| s), 0..4), // argv
    )
        .prop_map(|(pid, ppid, name, state, rss_kb, argv)| ProcessNode {
            pid,
            ppid,
            name,
            argv,
            state,
            rss_kb,
            children: vec![],
        })
}

/// Generate a process node with up to `max_depth` levels of children.
fn arb_process_node(max_depth: u32) -> impl Strategy<Value = ProcessNode> {
    arb_leaf_node().prop_flat_map(move |leaf| {
        if max_depth == 0 {
            Just(leaf).boxed()
        } else {
            proptest::collection::vec(arb_process_node(max_depth - 1), 0..3)
                .prop_map(move |children| {
                    let mut node = leaf.clone();
                    node.children = children;
                    node
                })
                .boxed()
        }
    })
}

/// Count processes and sum RSS in a tree (recursive helper).
fn walk_tree(node: &ProcessNode) -> (usize, u64) {
    let mut count = 1usize;
    let mut rss = node.rss_kb;
    for child in &node.children {
        let (c, r) = walk_tree(child);
        count += c;
        rss += r;
    }
    (count, rss)
}

/// Generate a complete ProcessTree with consistent metadata.
fn arb_process_tree() -> impl Strategy<Value = ProcessTree> {
    arb_process_node(3).prop_map(|root| {
        let (total_processes, total_rss_kb) = walk_tree(&root);
        ProcessTree {
            root,
            total_processes,
            total_rss_kb,
        }
    })
}

/// Generate a ProcessTree with only the root (no children) → always Idle.
fn arb_idle_tree() -> impl Strategy<Value = ProcessTree> {
    arb_leaf_node().prop_map(|root| {
        let rss = root.rss_kb;
        ProcessTree {
            root,
            total_processes: 1,
            total_rss_kb: rss,
        }
    })
}

fn arb_config() -> impl Strategy<Value = ProcessTreeConfig> {
    (any::<bool>(), 1..3600u64, 0..20u32, any::<bool>()).prop_map(
        |(enabled, interval, depth, threads)| ProcessTreeConfig {
            enabled,
            capture_interval_secs: interval,
            max_depth: depth,
            include_threads: threads,
        },
    )
}

// =============================================================================
// Serde roundtrip tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// ProcessState serde roundtrip.
    #[test]
    fn prop_process_state_serde_roundtrip(state in arb_process_state()) {
        let json = serde_json::to_string(&state).expect("serialize");
        let back: ProcessState = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(state, back);
    }

    /// PaneActivity serde roundtrip.
    #[test]
    fn prop_pane_activity_serde_roundtrip(activity in arb_pane_activity()) {
        let json = serde_json::to_string(&activity).expect("serialize");
        let back: PaneActivity = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(activity, back);
    }

    /// ProcessTree serde roundtrip preserves the entire tree structure.
    #[test]
    fn prop_process_tree_serde_roundtrip(tree in arb_process_tree()) {
        let json = serde_json::to_string(&tree).expect("serialize");
        let back: ProcessTree = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(tree, back);
    }

    /// ProcessTreeConfig serde roundtrip.
    #[test]
    fn prop_config_serde_roundtrip(cfg in arb_config()) {
        let json = serde_json::to_string(&cfg).expect("serialize");
        let back: ProcessTreeConfig = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(cfg.enabled, back.enabled);
        prop_assert_eq!(cfg.capture_interval_secs, back.capture_interval_secs);
        prop_assert_eq!(cfg.max_depth, back.max_depth);
        prop_assert_eq!(cfg.include_threads, back.include_threads);
    }
}

// =============================================================================
// Display implementations
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// ProcessState Display is non-empty and lowercase.
    #[test]
    fn prop_process_state_display_nonempty_lowercase(state in arb_process_state()) {
        let display = format!("{}", state);
        prop_assert!(!display.is_empty(), "Display must be non-empty");
        let lower = display.to_lowercase();
        prop_assert_eq!(
            display, lower,
            "Display must be lowercase"
        );
    }

    /// PaneActivity Display is non-empty and lowercase.
    #[test]
    fn prop_pane_activity_display_nonempty_lowercase(activity in arb_pane_activity()) {
        let display = format!("{}", activity);
        prop_assert!(!display.is_empty(), "Display must be non-empty");
        let lower = display.to_lowercase();
        prop_assert_eq!(
            display, lower,
            "Display must be lowercase"
        );
    }
}

// =============================================================================
// infer_activity: priority ordering
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// A tree with no children always infers Idle.
    #[test]
    fn prop_no_children_is_idle(tree in arb_idle_tree()) {
        prop_assert_eq!(
            infer_activity(&tree),
            PaneActivity::Idle,
            "No children must mean Idle"
        );
    }

    /// Per-name priority: agent check beats compile check for the same name.
    /// infer_activity uses first-match-wins over tree traversal order, so
    /// agent as the sole child always produces AgentRunning.
    #[test]
    fn prop_agent_sole_child_detected(
        agent_name in prop_oneof![
            Just("claude"), Just("claude-code"), Just("codex"),
            Just("gemini"), Just("aider"), Just("copilot"),
        ],
        state in arb_process_state(),
    ) {
        let tree = ProcessTree {
            root: ProcessNode {
                pid: 1,
                ppid: 0,
                name: "bash".into(),
                argv: vec![],
                state: ProcessState::Sleeping,
                rss_kb: 1000,
                children: vec![ProcessNode {
                    pid: 2,
                    ppid: 1,
                    name: agent_name.to_string(),
                    argv: vec![],
                    state,
                    rss_kb: 5000,
                    children: vec![],
                }],
            },
            total_processes: 2,
            total_rss_kb: 6000,
        };
        prop_assert_eq!(
            infer_activity(&tree),
            PaneActivity::AgentRunning,
            "Agent '{}' as sole child must infer AgentRunning",
            agent_name
        );
    }

    /// When agent appears before other tools in tree traversal, agent wins.
    #[test]
    fn prop_agent_before_compiler_wins(
        agent_name in prop_oneof![
            Just("claude"), Just("codex"), Just("gemini"),
        ],
    ) {
        let tree = ProcessTree {
            root: ProcessNode {
                pid: 1,
                ppid: 0,
                name: "bash".into(),
                argv: vec![],
                state: ProcessState::Sleeping,
                rss_kb: 1000,
                children: vec![
                    // Agent appears first in children → traversed first
                    ProcessNode {
                        pid: 2,
                        ppid: 1,
                        name: agent_name.to_string(),
                        argv: vec![],
                        state: ProcessState::Running,
                        rss_kb: 5000,
                        children: vec![],
                    },
                    ProcessNode {
                        pid: 3,
                        ppid: 1,
                        name: "cargo".into(),
                        argv: vec![],
                        state: ProcessState::Running,
                        rss_kb: 10000,
                        children: vec![],
                    },
                ],
            },
            total_processes: 3,
            total_rss_kb: 16000,
        };
        prop_assert_eq!(
            infer_activity(&tree),
            PaneActivity::AgentRunning,
            "Agent '{}' appearing before cargo must win",
            agent_name
        );
    }

    /// Compilation tools infer Compiling (when no agent present).
    #[test]
    fn prop_compiler_infers_compiling(
        compiler in prop_oneof![
            Just("cargo"), Just("rustc"), Just("gcc"), Just("g++"),
            Just("clang"), Just("make"), Just("cmake"), Just("ninja"),
            Just("javac"), Just("tsc"), Just("esbuild"), Just("webpack"),
            Just("vite"),
        ],
    ) {
        let tree = ProcessTree {
            root: ProcessNode {
                pid: 1,
                ppid: 0,
                name: "bash".into(),
                argv: vec![],
                state: ProcessState::Sleeping,
                rss_kb: 1000,
                children: vec![ProcessNode {
                    pid: 2,
                    ppid: 1,
                    name: compiler.to_string(),
                    argv: vec![],
                    state: ProcessState::Running,
                    rss_kb: 10000,
                    children: vec![],
                }],
            },
            total_processes: 2,
            total_rss_kb: 11000,
        };
        prop_assert_eq!(
            infer_activity(&tree),
            PaneActivity::Compiling,
            "Compiler '{}' must infer Compiling", compiler
        );
    }

    /// Testing tools infer Testing (when no agent or compiler present).
    #[test]
    fn prop_test_runner_infers_testing(
        runner in prop_oneof![
            Just("pytest"), Just("jest"), Just("mocha"),
            Just("vitest"), Just("cargo-nextest"),
        ],
    ) {
        let tree = ProcessTree {
            root: ProcessNode {
                pid: 1,
                ppid: 0,
                name: "bash".into(),
                argv: vec![],
                state: ProcessState::Sleeping,
                rss_kb: 1000,
                children: vec![ProcessNode {
                    pid: 2,
                    ppid: 1,
                    name: runner.to_string(),
                    argv: vec![],
                    state: ProcessState::Running,
                    rss_kb: 10000,
                    children: vec![],
                }],
            },
            total_processes: 2,
            total_rss_kb: 11000,
        };
        prop_assert_eq!(
            infer_activity(&tree),
            PaneActivity::Testing,
            "Test runner '{}' must infer Testing", runner
        );
    }

    /// VCS tools infer VersionControl (when no higher-priority process present).
    #[test]
    fn prop_vcs_infers_version_control(
        vcs in prop_oneof![Just("git"), Just("hg"), Just("svn"), Just("gh")],
    ) {
        let tree = ProcessTree {
            root: ProcessNode {
                pid: 1,
                ppid: 0,
                name: "bash".into(),
                argv: vec![],
                state: ProcessState::Sleeping,
                rss_kb: 1000,
                children: vec![ProcessNode {
                    pid: 2,
                    ppid: 1,
                    name: vcs.to_string(),
                    argv: vec![],
                    state: ProcessState::Running,
                    rss_kb: 1000,
                    children: vec![],
                }],
            },
            total_processes: 2,
            total_rss_kb: 2000,
        };
        prop_assert_eq!(
            infer_activity(&tree),
            PaneActivity::VersionControl,
            "VCS tool '{}' must infer VersionControl", vcs
        );
    }

    /// Editors infer Editing (when no higher-priority process present).
    #[test]
    fn prop_editor_infers_editing(
        editor in prop_oneof![
            Just("vim"), Just("nvim"), Just("emacs"), Just("nano"),
            Just("code"), Just("helix"), Just("hx"), Just("rust-analyzer"),
            Just("gopls"), Just("pyright"),
        ],
    ) {
        let tree = ProcessTree {
            root: ProcessNode {
                pid: 1,
                ppid: 0,
                name: "bash".into(),
                argv: vec![],
                state: ProcessState::Sleeping,
                rss_kb: 1000,
                children: vec![ProcessNode {
                    pid: 2,
                    ppid: 1,
                    name: editor.to_string(),
                    argv: vec![],
                    state: ProcessState::Running,
                    rss_kb: 10000,
                    children: vec![],
                }],
            },
            total_processes: 2,
            total_rss_kb: 11000,
        };
        prop_assert_eq!(
            infer_activity(&tree),
            PaneActivity::Editing,
            "Editor '{}' must infer Editing", editor
        );
    }

    /// Unknown process names with children infer Active.
    #[test]
    fn prop_unknown_process_infers_active(
        name in "[a-z]{3,8}-custom-tool",
    ) {
        let tree = ProcessTree {
            root: ProcessNode {
                pid: 1,
                ppid: 0,
                name: "bash".into(),
                argv: vec![],
                state: ProcessState::Sleeping,
                rss_kb: 1000,
                children: vec![ProcessNode {
                    pid: 2,
                    ppid: 1,
                    name,
                    argv: vec![],
                    state: ProcessState::Running,
                    rss_kb: 1000,
                    children: vec![],
                }],
            },
            total_processes: 2,
            total_rss_kb: 2000,
        };
        prop_assert_eq!(
            infer_activity(&tree),
            PaneActivity::Active,
            "Unknown tool must infer Active"
        );
    }
}

// =============================================================================
// exe_names(): sorted, deduplicated, complete
// =============================================================================

/// Collect all names from a ProcessNode recursively.
fn collect_all_names(node: &ProcessNode) -> Vec<String> {
    let mut names = vec![node.name.clone()];
    for child in &node.children {
        names.extend(collect_all_names(child));
    }
    names
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// exe_names() returns a sorted list.
    #[test]
    fn prop_exe_names_sorted(tree in arb_process_tree()) {
        let names = tree.exe_names();
        let mut sorted = names.clone();
        sorted.sort();
        prop_assert_eq!(names, sorted, "exe_names must be sorted");
    }

    /// exe_names() has no duplicates.
    #[test]
    fn prop_exe_names_deduplicated(tree in arb_process_tree()) {
        let names = tree.exe_names();
        let mut deduped = names.clone();
        deduped.dedup();
        prop_assert_eq!(names, deduped, "exe_names must have no duplicates");
    }

    /// exe_names() contains all names from the tree.
    #[test]
    fn prop_exe_names_complete(tree in arb_process_tree()) {
        let exe_names = tree.exe_names();
        let all_names = collect_all_names(&tree.root);
        for name in &all_names {
            prop_assert!(
                exe_names.contains(name),
                "exe_names must contain '{}' from tree",
                name
            );
        }
    }

    /// exe_names() length <= total_processes (since deduplication can shrink).
    #[test]
    fn prop_exe_names_bounded(tree in arb_process_tree()) {
        let names = tree.exe_names();
        prop_assert!(
            names.len() <= tree.total_processes,
            "exe_names count ({}) must be <= total_processes ({})",
            names.len(), tree.total_processes
        );
    }
}

// =============================================================================
// contains_process(): consistent with exe_names()
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// contains_process() agrees with exe_names() for all names in the tree.
    #[test]
    fn prop_contains_process_consistent_with_exe_names(tree in arb_process_tree()) {
        let names = tree.exe_names();
        for name in &names {
            prop_assert!(
                tree.contains_process(name),
                "contains_process('{}') must be true for name in exe_names()",
                name
            );
        }
    }

    /// contains_process() returns false for names not in the tree.
    #[test]
    fn prop_contains_process_false_for_absent(tree in arb_process_tree()) {
        let sentinel = "ZZZZZ_NONEXISTENT_PROCESS_ZZZZZ";
        prop_assert!(
            !tree.contains_process(sentinel),
            "contains_process must be false for absent process"
        );
    }
}

// =============================================================================
// subtree_rss_kb(): recursive aggregation consistency
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Root subtree_rss_kb equals manually computed sum of all nodes.
    #[test]
    fn prop_subtree_rss_matches_walk(tree in arb_process_tree()) {
        let (_, rss) = walk_tree(&tree.root);
        prop_assert_eq!(
            tree.root.subtree_rss_kb(), rss,
            "subtree_rss_kb must equal walk_tree RSS"
        );
    }

    /// subtree_rss_kb equals total_rss_kb for a well-formed tree.
    #[test]
    fn prop_subtree_rss_matches_total(tree in arb_process_tree()) {
        prop_assert_eq!(
            tree.root.subtree_rss_kb(), tree.total_rss_kb,
            "root subtree_rss_kb ({}) must equal total_rss_kb ({})",
            tree.root.subtree_rss_kb(), tree.total_rss_kb
        );
    }

    /// Leaf nodes have subtree_rss_kb == self.rss_kb.
    #[test]
    fn prop_leaf_subtree_rss_is_own_rss(node in arb_leaf_node()) {
        prop_assert_eq!(
            node.subtree_rss_kb(), node.rss_kb,
            "Leaf subtree_rss must equal own rss_kb"
        );
    }

    /// subtree_rss_kb >= own rss_kb (children can only add).
    #[test]
    fn prop_subtree_rss_gte_own_rss(tree in arb_process_tree()) {
        prop_assert!(
            tree.root.subtree_rss_kb() >= tree.root.rss_kb,
            "subtree_rss ({}) must be >= own rss ({})",
            tree.root.subtree_rss_kb(), tree.root.rss_kb
        );
    }
}

// =============================================================================
// count_tree: metadata consistency
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// total_processes matches manual walk count (for well-formed trees).
    #[test]
    fn prop_total_processes_matches_walk(tree in arb_process_tree()) {
        let (count, _) = walk_tree(&tree.root);
        prop_assert_eq!(
            tree.total_processes, count,
            "total_processes ({}) must match walk count ({})",
            tree.total_processes, count
        );
    }

    /// total_rss_kb matches manual walk sum (for well-formed trees).
    #[test]
    fn prop_total_rss_matches_walk(tree in arb_process_tree()) {
        let (_, rss) = walk_tree(&tree.root);
        prop_assert_eq!(
            tree.total_rss_kb, rss,
            "total_rss_kb ({}) must match walk sum ({})",
            tree.total_rss_kb, rss
        );
    }

    /// total_processes >= 1 (at least the root).
    #[test]
    fn prop_total_processes_at_least_one(tree in arb_process_tree()) {
        prop_assert!(
            tree.total_processes >= 1,
            "total_processes must be >= 1"
        );
    }
}

// =============================================================================
// Deep tree: activity inferred from nested processes
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Agent name nested deep in the tree still gets detected.
    #[test]
    fn prop_deep_agent_detected(
        agent_name in prop_oneof![
            Just("claude"), Just("codex"), Just("gemini"),
        ],
        depth in 1..5usize,
    ) {
        // Build a chain: bash → node → ... → agent
        let mut inner = ProcessNode {
            pid: (depth + 2) as u32,
            ppid: (depth + 1) as u32,
            name: agent_name.to_string(),
            argv: vec![],
            state: ProcessState::Running,
            rss_kb: 1000,
            children: vec![],
        };

        for i in (1..=depth).rev() {
            inner = ProcessNode {
                pid: (i + 1) as u32,
                ppid: i as u32,
                name: format!("wrapper-{}", i),
                argv: vec![],
                state: ProcessState::Sleeping,
                rss_kb: 500,
                children: vec![inner],
            };
        }

        let root = ProcessNode {
            pid: 1,
            ppid: 0,
            name: "bash".into(),
            argv: vec![],
            state: ProcessState::Sleeping,
            rss_kb: 1000,
            children: vec![inner],
        };

        let (total_processes, total_rss_kb) = walk_tree(&root);
        let tree = ProcessTree {
            root,
            total_processes,
            total_rss_kb,
        };

        prop_assert_eq!(
            infer_activity(&tree),
            PaneActivity::AgentRunning,
            "Agent '{}' at depth {} must still be detected",
            agent_name, depth
        );
    }
}
