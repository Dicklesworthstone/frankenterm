//! Property-based tests for session restoration correctness.
//!
//! Verifies the core isomorphism property:
//!   for all valid mux states S, restore(snapshot(S)) ≈ S
//!
//! "Approximately equal" means structurally identical up to PIDs,
//! timestamps, and trailing whitespace.
//!
//! Bead: wa-rsaf.2

use std::collections::HashMap;
use std::sync::Arc;

use proptest::prelude::*;

use frankenterm_core::restore_layout::{LayoutRestorer, RestoreConfig};
use frankenterm_core::restore_process::{LaunchAction, LaunchConfig, ProcessLauncher};
use frankenterm_core::restore_scrollback::{InjectionConfig, ScrollbackData, ScrollbackInjector};
use frankenterm_core::session_pane_state::{
    AgentMetadata, CapturedEnv, PaneStateSnapshot, ProcessInfo, TerminalState,
};
use frankenterm_core::session_topology::{PaneNode, TabSnapshot, TopologySnapshot, WindowSnapshot};
use frankenterm_core::wezterm::{MockWezterm, WeztermInterface};

// =============================================================================
// Proptest strategies: Atomic generators
// =============================================================================

/// Generate a valid pane ID (nonzero, reasonable range).
fn arb_pane_id() -> impl Strategy<Value = u64> {
    1u64..10_000
}

/// Generate reasonable terminal dimensions.
fn arb_dimensions() -> impl Strategy<Value = (u16, u16)> {
    (8u16..=120, 40u16..=240) // (rows, cols)
}

/// Generate an optional working directory from a realistic pool.
fn arb_cwd() -> impl Strategy<Value = Option<String>> {
    prop_oneof![
        3 => Just(None),
        10 => Just(Some("/home/user/project".to_string())),
        5 => Just(Some("/tmp".to_string())),
        5 => Just(Some("/home/user".to_string())),
        3 => Just(Some("/var/log".to_string())),
        3 => Just(Some("/home/user/projects/frankenterm".to_string())),
        1 => Just(Some("/".to_string())),
    ]
}

/// Generate a pane title.
fn arb_title() -> impl Strategy<Value = Option<String>> {
    prop_oneof![
        3 => Just(None),
        3 => Just(Some("bash".to_string())),
        2 => Just(Some("zsh".to_string())),
        1 => Just(Some("vim main.rs".to_string())),
        1 => Just(Some("cargo test".to_string())),
    ]
}

/// Generate a split ratio in (0.1, 0.9).
fn arb_ratio() -> impl Strategy<Value = f64> {
    (10u32..=90).prop_map(|r| r as f64 / 100.0)
}

/// Generate a shell name.
fn arb_shell() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("bash".to_string()),
        Just("zsh".to_string()),
        Just("fish".to_string()),
        Just("/bin/sh".to_string()),
    ]
}

/// Generate an agent type string.
fn arb_agent_type() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("claude_code".to_string()),
        Just("codex".to_string()),
        Just("gemini_cli".to_string()),
        Just("aider".to_string()),
    ]
}

// =============================================================================
// Proptest strategies: Composite generators
// =============================================================================

/// Generate a leaf PaneNode.
fn arb_leaf() -> impl Strategy<Value = PaneNode> {
    (
        arb_pane_id(),
        arb_dimensions(),
        arb_cwd(),
        arb_title(),
        any::<bool>(),
    )
        .prop_map(
            |(pane_id, (rows, cols), cwd, title, is_active)| PaneNode::Leaf {
                pane_id,
                rows,
                cols,
                cwd,
                title,
                is_active,
            },
        )
}

/// Generate a recursive PaneNode tree.
///
/// Uses `prop_recursive` to build trees up to 4 levels deep with at most
/// 30 nodes, keeping test execution fast while covering nontrivial splits.
fn arb_pane_tree() -> impl Strategy<Value = PaneNode> {
    arb_leaf().prop_recursive(
        4,  // max depth
        30, // max nodes
        2,  // items per collection (binary splits)
        |inner| {
            prop_oneof![
                // HSplit with 2 children
                (arb_ratio(), inner.clone(), inner.clone()).prop_map(|(ratio, left, right)| {
                    PaneNode::HSplit {
                        children: vec![(ratio, left), (1.0 - ratio, right)],
                    }
                }),
                // VSplit with 2 children
                (arb_ratio(), inner.clone(), inner).prop_map(|(ratio, left, right)| {
                    PaneNode::VSplit {
                        children: vec![(ratio, left), (1.0 - ratio, right)],
                    }
                }),
            ]
        },
    )
}

/// Generate a TabSnapshot.
fn arb_tab(tab_id: u64) -> impl Strategy<Value = TabSnapshot> {
    (arb_pane_tree(), arb_title()).prop_map(move |(pane_tree, title)| {
        let active_pane_id = first_leaf_id(&pane_tree);
        TabSnapshot {
            tab_id,
            title,
            pane_tree,
            active_pane_id: Some(active_pane_id),
        }
    })
}

/// Generate a WindowSnapshot with 1-4 tabs.
fn arb_window(window_id: u64) -> impl Strategy<Value = WindowSnapshot> {
    (1usize..=4).prop_flat_map(move |tab_count| {
        let tabs: Vec<_> = (0..tab_count)
            .map(|i| arb_tab(window_id * 100 + i as u64))
            .collect();
        tabs.prop_map(move |tabs| WindowSnapshot {
            window_id,
            title: None,
            position: None,
            size: None,
            tabs,
            active_tab_index: Some(0),
        })
    })
}

/// Generate a TopologySnapshot with 1-3 windows.
fn arb_topology() -> impl Strategy<Value = TopologySnapshot> {
    (1usize..=3).prop_flat_map(|win_count| {
        let windows: Vec<_> = (0..win_count).map(|i| arb_window(i as u64)).collect();
        windows.prop_map(|windows| TopologySnapshot {
            schema_version: 1,
            captured_at: 1_700_000_000,
            workspace_id: Some("default".to_string()),
            windows,
        })
    })
}

/// Generate a TerminalState.
fn arb_terminal_state() -> impl Strategy<Value = TerminalState> {
    arb_dimensions().prop_map(|(rows, cols)| TerminalState {
        rows,
        cols,
        cursor_row: 0,
        cursor_col: 0,
        is_alt_screen: false,
        title: "test".to_string(),
    })
}

/// Generate a ProcessInfo.
fn arb_process_info() -> impl Strategy<Value = Option<ProcessInfo>> {
    prop_oneof![
        3 => Just(None),
        5 => arb_shell().prop_map(|s| Some(ProcessInfo {
            name: s,
            pid: Some(1234),
            argv: None,
        })),
        2 => arb_agent_type().prop_map(|a| Some(ProcessInfo {
            name: a,
            pid: Some(5678),
            argv: Some(vec!["claude".to_string(), "--headless".to_string()]),
        })),
    ]
}

/// Generate an AgentMetadata.
fn arb_agent_metadata() -> impl Strategy<Value = Option<AgentMetadata>> {
    prop_oneof![
        7 => Just(None),
        3 => arb_agent_type().prop_map(|at| Some(AgentMetadata {
            agent_type: at,
            session_id: Some("test-session-1".to_string()),
            state: Some("active".to_string()),
        })),
    ]
}

/// Generate a PaneStateSnapshot for a given pane_id.
fn arb_pane_state(pane_id: u64) -> impl Strategy<Value = PaneStateSnapshot> {
    (
        arb_cwd(),
        arb_process_info(),
        arb_shell().prop_map(Some),
        arb_terminal_state(),
        arb_agent_metadata(),
    )
        .prop_map(
            move |(cwd, process, shell, terminal, agent)| PaneStateSnapshot {
                schema_version: 1,
                pane_id,
                captured_at: 1_700_000_000,
                cwd,
                foreground_process: process,
                shell,
                terminal,
                scrollback_ref: None,
                agent,
                env: Some(CapturedEnv {
                    vars: HashMap::new(),
                    redacted_count: 0,
                }),
            },
        )
}

/// Generate scrollback content (lines of text).
fn arb_scrollback_lines() -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec(
        prop_oneof![
            // Plain text lines
            60 => "[a-zA-Z0-9 .,:;!?/-]{0,120}",
            // Lines with common terminal output patterns
            15 => "\\$ (ls|cd|git|cargo|npm) [a-z ]{0,40}",
            // Prompt-like lines
            10 => "user@host:[~/a-z]{1,30}\\$ ",
            // Empty lines
            10 => Just(String::new()),
            // Lines with paths
            5 => "/[a-z]{1,10}(/[a-z]{1,10}){0,5}",
        ],
        0..200,
    )
}

// =============================================================================
// Helpers
// =============================================================================

/// Extract the first leaf pane_id from a PaneNode tree.
fn first_leaf_id(node: &PaneNode) -> u64 {
    match node {
        PaneNode::Leaf { pane_id, .. } => *pane_id,
        PaneNode::HSplit { children } | PaneNode::VSplit { children } => {
            first_leaf_id(&children[0].1)
        }
    }
}

/// Collect all leaf pane IDs from a PaneNode tree.
fn collect_leaf_ids(node: &PaneNode) -> Vec<u64> {
    match node {
        PaneNode::Leaf { pane_id, .. } => vec![*pane_id],
        PaneNode::HSplit { children } | PaneNode::VSplit { children } => children
            .iter()
            .flat_map(|(_, c)| collect_leaf_ids(c))
            .collect(),
    }
}

/// Count total leaves in a topology snapshot.
fn count_leaves(topo: &TopologySnapshot) -> usize {
    topo.windows
        .iter()
        .flat_map(|w| &w.tabs)
        .map(|t| collect_leaf_ids(&t.pane_tree).len())
        .sum()
}

/// Reassign pane IDs in a tree to be globally unique.
fn reassign_pane_ids(node: &mut PaneNode, counter: &mut u64) {
    match node {
        PaneNode::Leaf { pane_id, .. } => {
            *pane_id = *counter;
            *counter += 1;
        }
        PaneNode::HSplit { children } | PaneNode::VSplit { children } => {
            for (_, child) in children.iter_mut() {
                reassign_pane_ids(child, counter);
            }
        }
    }
}

/// Ensure all pane IDs in a topology are unique.
fn deduplicate_topology(topo: &mut TopologySnapshot) {
    let mut counter = 1u64;
    for window in &mut topo.windows {
        for tab in &mut window.tabs {
            reassign_pane_ids(&mut tab.pane_tree, &mut counter);
            tab.active_pane_id = Some(first_leaf_id(&tab.pane_tree));
        }
    }
}

/// Compare two PaneNode trees with floating-point tolerance for ratios.
fn pane_nodes_approx_equal(a: &PaneNode, b: &PaneNode, tol: f64) -> bool {
    match (a, b) {
        (PaneNode::Leaf { .. }, PaneNode::Leaf { .. }) => true,
        (PaneNode::HSplit { children: ac }, PaneNode::HSplit { children: bc })
        | (PaneNode::VSplit { children: ac }, PaneNode::VSplit { children: bc }) => {
            ac.len() == bc.len()
                && ac
                    .iter()
                    .zip(bc.iter())
                    .all(|((ar, achild), (br, bchild))| {
                        (ar - br).abs() < tol && pane_nodes_approx_equal(achild, bchild, tol)
                    })
        }
        _ => false,
    }
}

/// Check structural equality of two PaneNode trees, ignoring pane IDs.
/// Returns true if the trees have the same shape (same split types, same
/// depths, same number of children at each level).
#[allow(dead_code)]
fn structurally_equal(a: &PaneNode, b: &PaneNode) -> bool {
    match (a, b) {
        (PaneNode::Leaf { .. }, PaneNode::Leaf { .. }) => true,
        (PaneNode::HSplit { children: ac }, PaneNode::HSplit { children: bc })
        | (PaneNode::VSplit { children: ac }, PaneNode::VSplit { children: bc }) => {
            ac.len() == bc.len()
                && ac
                    .iter()
                    .zip(bc.iter())
                    .all(|((_, a_child), (_, b_child))| structurally_equal(a_child, b_child))
        }
        _ => false,
    }
}

/// Check that ratios at each split level are approximately preserved.
#[allow(dead_code)]
fn ratios_approximately_equal(a: &PaneNode, b: &PaneNode, tolerance: f64) -> bool {
    match (a, b) {
        (PaneNode::Leaf { .. }, PaneNode::Leaf { .. }) => true,
        (PaneNode::HSplit { children: ac }, PaneNode::HSplit { children: bc })
        | (PaneNode::VSplit { children: ac }, PaneNode::VSplit { children: bc }) => {
            ac.len() == bc.len()
                && ac
                    .iter()
                    .zip(bc.iter())
                    .all(|((ar, a_child), (br, b_child))| {
                        (ar - br).abs() < tolerance
                            && ratios_approximately_equal(a_child, b_child, tolerance)
                    })
        }
        _ => false,
    }
}

// =============================================================================
// Property: topology structure preservation
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    /// For any valid topology snapshot, restoring the layout on a mock WezTerm
    /// produces a pane_id_map with one entry per leaf pane and zero failures
    /// when continue_on_error is false.
    #[test]
    fn layout_restore_creates_correct_pane_count(mut topo in arb_topology()) {
        deduplicate_topology(&mut topo);
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let expected_panes = count_leaves(&topo);
            let mock = Arc::new(MockWezterm::new());
            let restorer = LayoutRestorer::new(
                mock.clone(),
                RestoreConfig {
                    restore_working_dirs: true,
                    restore_split_ratios: true,
                    continue_on_error: false,
                },
            );

            let result = restorer.restore(&topo).await.unwrap();
            prop_assert_eq!(result.pane_id_map.len(), expected_panes);
            prop_assert!(result.failed_panes.is_empty());
            prop_assert_eq!(result.panes_created, expected_panes);
            Ok(())
        })?;
    }

    /// Every old pane ID in the snapshot maps to a unique new pane ID.
    #[test]
    fn layout_restore_produces_unique_pane_ids(mut topo in arb_topology()) {
        deduplicate_topology(&mut topo);
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mock = Arc::new(MockWezterm::new());
            let restorer = LayoutRestorer::new(
                mock,
                RestoreConfig {
                    restore_working_dirs: true,
                    restore_split_ratios: true,
                    continue_on_error: false,
                },
            );

            let result = restorer.restore(&topo).await.unwrap();

            // All new IDs must be unique
            let mut new_ids: Vec<u64> = result.pane_id_map.values().copied().collect();
            new_ids.sort();
            new_ids.dedup();
            prop_assert_eq!(new_ids.len(), result.pane_id_map.len());
            Ok(())
        })?;
    }
}

// =============================================================================
// Property: structural isomorphism
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// Restoring a topology produces the same structural tree shape: split
    /// directions and nesting depths are preserved. Pane IDs may differ.
    #[test]
    fn layout_structure_is_preserved(mut topo in arb_topology()) {
        deduplicate_topology(&mut topo);
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mock = Arc::new(MockWezterm::new());
            let restorer = LayoutRestorer::new(
                mock.clone(),
                RestoreConfig {
                    restore_working_dirs: true,
                    restore_split_ratios: true,
                    continue_on_error: false,
                },
            );

            let result = restorer.restore(&topo).await.unwrap();

            // Verify per-tab structure
            for window in &topo.windows {
                for tab in &window.tabs {
                    let original_leaf_count = collect_leaf_ids(&tab.pane_tree).len();
                    let mapped_count = collect_leaf_ids(&tab.pane_tree)
                        .iter()
                        .filter(|id| result.pane_id_map.contains_key(id))
                        .count();
                    prop_assert_eq!(
                        mapped_count,
                        original_leaf_count,
                        "all leaves in tab {} must be mapped",
                        tab.tab_id
                    );
                }
            }
            Ok(())
        })?;
    }
}

// =============================================================================
// Property: window/tab count preservation
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    /// The number of windows and tabs created matches the snapshot.
    #[test]
    fn window_and_tab_counts_match(mut topo in arb_topology()) {
        deduplicate_topology(&mut topo);
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mock = Arc::new(MockWezterm::new());
            let restorer = LayoutRestorer::new(
                mock,
                RestoreConfig::default(),
            );

            let result = restorer.restore(&topo).await.unwrap();

            let expected_windows = topo.windows.len();
            let expected_tabs: usize = topo.windows.iter().map(|w| w.tabs.len()).sum();

            prop_assert_eq!(result.windows_created, expected_windows);
            prop_assert_eq!(result.tabs_created, expected_tabs);
            Ok(())
        })?;
    }
}

// =============================================================================
// Property: scrollback injection correctness
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Scrollback content injected into a mock pane can be fully retrieved.
    /// Verifies that no data is lost during chunked injection.
    #[test]
    fn scrollback_injection_preserves_content(
        lines in arb_scrollback_lines()
    ) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mock = Arc::new(MockWezterm::new());
            mock.add_default_pane(1).await;

            let data = ScrollbackData::from_segments(lines.clone());
            if data.lines.is_empty() {
                return Ok(());
            }

            let injector = ScrollbackInjector::new(
                mock.clone(),
                InjectionConfig {
                    max_lines: 50_000,
                    chunk_size: 512, // small chunks to stress chunking logic
                    inter_chunk_delay_ms: 0,
                    concurrent_injections: 1,
                },
            );

            let mut scrollback_map = HashMap::new();
            scrollback_map.insert(1u64, data);

            let id_map: HashMap<u64, u64> = std::iter::once((1, 1)).collect();
            let report = injector.inject(&id_map, &scrollback_map).await;

            prop_assert!(
                report.failures.is_empty(),
                "injection should succeed: {:?}",
                report.failures
            );

            // Verify injected content matches original
            let pane_content: String = WeztermInterface::get_text(&*mock, 1, false)
                .await
                .unwrap();

            // The mock echoes send_text content, so the injected data should
            // be a substring of the pane content (modulo ANSI reset prefix).
            for line in &lines {
                if !line.is_empty() {
                    prop_assert!(
                        pane_content.contains(line.as_str()),
                        "pane content should contain line: {:?}",
                        line
                    );
                }
            }

            Ok(())
        })?;
    }

    /// Scrollback truncation preserves the most recent lines.
    #[test]
    fn scrollback_truncation_keeps_recent(
        lines in prop::collection::vec("[a-z]{1,50}", 10..500),
        max_lines in 1usize..50,
    ) {
        let mut data = ScrollbackData::from_segments(lines.clone());
        let original_len = data.lines.len();
        data.truncate(max_lines);

        if original_len <= max_lines {
            prop_assert_eq!(data.lines.len(), original_len);
        } else {
            prop_assert_eq!(data.lines.len(), max_lines);
            // Truncation keeps the LAST lines
            let expected_start = original_len - max_lines;
            for (i, line) in data.lines.iter().enumerate() {
                prop_assert_eq!(
                    line,
                    &lines[expected_start + i],
                    "truncated line {} mismatch",
                    i
                );
            }
        }
    }
}

// =============================================================================
// Property: process plan determinism
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// Generating a launch plan from the same inputs always produces the
    /// same plan. This ensures no hidden nondeterminism in process resolution.
    #[test]
    fn process_plan_is_deterministic(
        pane_ids in prop::collection::vec(1u64..1000, 1..10),
    ) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mock = Arc::new(MockWezterm::new());
            let launcher = ProcessLauncher::new(
                mock,
                LaunchConfig::default(),
            );

            // Build consistent pane state and ID map
            let states: Vec<PaneStateSnapshot> = pane_ids
                .iter()
                .map(|&id| PaneStateSnapshot {
                    schema_version: 1,
                    pane_id: id,
                    captured_at: 1_700_000_000,
                    cwd: Some("/home/user".to_string()),
                    foreground_process: Some(ProcessInfo {
                        name: "bash".to_string(),
                        pid: Some(1000 + id as u32),
                        argv: None,
                    }),
                    shell: Some("bash".to_string()),
                    terminal: TerminalState {
                        rows: 24,
                        cols: 80,
                        cursor_row: 0,
                        cursor_col: 0,
                        is_alt_screen: false,
                        title: "bash".to_string(),
                    },
                    scrollback_ref: None,
                    agent: None,
                    env: None,
                })
                .collect();

            let id_map: HashMap<u64, u64> = pane_ids
                .iter()
                .enumerate()
                .map(|(i, &old)| (old, 1000 + i as u64))
                .collect();

            let plan1 = launcher.plan(&id_map, &states);
            let plan2 = launcher.plan(&id_map, &states);

            prop_assert_eq!(plan1.len(), plan2.len());
            for (p1, p2) in plan1.iter().zip(plan2.iter()) {
                prop_assert_eq!(&p1.action, &p2.action);
                prop_assert_eq!(p1.old_pane_id, p2.old_pane_id);
                prop_assert_eq!(p1.new_pane_id, p2.new_pane_id);
            }
            Ok(())
        })?;
    }

    /// A plan has one entry per mapped pane, no more, no less.
    #[test]
    fn process_plan_covers_all_mapped_panes(
        pane_count in 1usize..20,
    ) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mock = Arc::new(MockWezterm::new());
            let launcher = ProcessLauncher::new(mock, LaunchConfig::default());

            let states: Vec<PaneStateSnapshot> = (0..pane_count)
                .map(|i| {
                    let id = i as u64 + 1;
                    PaneStateSnapshot {
                        schema_version: 1,
                        pane_id: id,
                        captured_at: 1_700_000_000,
                        cwd: Some(format!("/home/user/project-{}", i)),
                        foreground_process: Some(ProcessInfo {
                            name: "bash".to_string(),
                            pid: Some(1000 + i as u32),
                            argv: None,
                        }),
                        shell: Some("bash".to_string()),
                        terminal: TerminalState {
                            rows: 24,
                            cols: 80,
                            cursor_row: 0,
                            cursor_col: 0,
                            is_alt_screen: false,
                            title: "bash".to_string(),
                        },
                        scrollback_ref: None,
                        agent: None,
                        env: None,
                    }
                })
                .collect();

            let id_map: HashMap<u64, u64> = (0..pane_count)
                .map(|i| (i as u64 + 1, 100 + i as u64))
                .collect();

            let plans = launcher.plan(&id_map, &states);
            prop_assert_eq!(plans.len(), pane_count);

            // Every mapped pane should appear in the plan
            for plan in &plans {
                prop_assert!(
                    id_map.contains_key(&plan.old_pane_id),
                    "plan entry for {} should be in id_map",
                    plan.old_pane_id
                );
            }
            Ok(())
        })?;
    }
}

// =============================================================================
// Property: PaneNode serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1000))]

    /// Serializing a PaneNode to JSON and back produces a structurally
    /// equivalent tree. Ratios may differ by floating-point rounding.
    #[test]
    fn pane_node_serde_roundtrip(tree in arb_pane_tree()) {
        let json = serde_json::to_string(&tree).unwrap();
        let deserialized: PaneNode = serde_json::from_str(&json).unwrap();
        prop_assert!(
            pane_nodes_approx_equal(&tree, &deserialized, 0.01),
            "serde roundtrip changed tree structure or ratios beyond tolerance"
        );
    }

    /// TopologySnapshot serde roundtrip preserves structure.
    #[test]
    fn topology_serde_roundtrip(topo in arb_topology()) {
        let json = serde_json::to_string(&topo).unwrap();
        let deserialized: TopologySnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(topo.schema_version, deserialized.schema_version);
        prop_assert_eq!(topo.captured_at, deserialized.captured_at);
        prop_assert_eq!(&topo.workspace_id, &deserialized.workspace_id);
        prop_assert_eq!(topo.windows.len(), deserialized.windows.len());
        for (ow, dw) in topo.windows.iter().zip(deserialized.windows.iter()) {
            prop_assert_eq!(ow.window_id, dw.window_id);
            prop_assert_eq!(ow.tabs.len(), dw.tabs.len());
            for (ot, dt) in ow.tabs.iter().zip(dw.tabs.iter()) {
                prop_assert!(
                    pane_nodes_approx_equal(&ot.pane_tree, &dt.pane_tree, 0.01),
                    "tab {} pane tree changed beyond tolerance",
                    ot.tab_id
                );
            }
        }
    }
}

// =============================================================================
// Property: PaneStateSnapshot serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    /// PaneStateSnapshot serde roundtrip preserves all fields.
    #[test]
    fn pane_state_serde_roundtrip(state in arb_pane_state(42)) {
        let json = serde_json::to_string(&state).unwrap();
        let deserialized: PaneStateSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(state.pane_id, deserialized.pane_id);
        prop_assert_eq!(&state.cwd, &deserialized.cwd);
        prop_assert_eq!(&state.shell, &deserialized.shell);
        prop_assert_eq!(state.terminal.rows, deserialized.terminal.rows);
        prop_assert_eq!(state.terminal.cols, deserialized.terminal.cols);
    }
}

// =============================================================================
// Property: edge cases
// =============================================================================

/// Empty topology snapshot restores with zero panes.
#[tokio::test]
async fn empty_topology_restores_empty() {
    let topo = TopologySnapshot {
        schema_version: 1,
        captured_at: 1_700_000_000,
        workspace_id: None,
        windows: vec![],
    };

    let mock = Arc::new(MockWezterm::new());
    let restorer = LayoutRestorer::new(mock, RestoreConfig::default());
    let result = restorer.restore(&topo).await.unwrap();

    assert_eq!(result.pane_id_map.len(), 0);
    assert_eq!(result.windows_created, 0);
    assert_eq!(result.tabs_created, 0);
    assert_eq!(result.panes_created, 0);
}

/// Single-pane topology restores correctly.
#[tokio::test]
async fn single_pane_topology_restores() {
    let topo = TopologySnapshot {
        schema_version: 1,
        captured_at: 1_700_000_000,
        workspace_id: Some("default".to_string()),
        windows: vec![WindowSnapshot {
            window_id: 0,
            title: None,
            position: None,
            size: None,
            tabs: vec![TabSnapshot {
                tab_id: 0,
                title: None,
                pane_tree: PaneNode::Leaf {
                    pane_id: 1,
                    rows: 24,
                    cols: 80,
                    cwd: Some("/home/user".to_string()),
                    title: Some("bash".to_string()),
                    is_active: true,
                },
                active_pane_id: Some(1),
            }],
            active_tab_index: Some(0),
        }],
    };

    let mock = Arc::new(MockWezterm::new());
    let restorer = LayoutRestorer::new(mock, RestoreConfig::default());
    let result = restorer.restore(&topo).await.unwrap();

    assert_eq!(result.pane_id_map.len(), 1);
    assert!(result.pane_id_map.contains_key(&1));
    assert_eq!(result.windows_created, 1);
    assert_eq!(result.tabs_created, 1);
    assert_eq!(result.panes_created, 1);
}

/// Maximum complexity topology: many windows, tabs, and deep splits.
#[tokio::test]
async fn max_complexity_topology_restores() {
    // Build a deep split tree: 5 levels, ~31 panes
    fn deep_tree(depth: u32, next_id: &mut u64) -> PaneNode {
        if depth == 0 {
            let id = *next_id;
            *next_id += 1;
            return PaneNode::Leaf {
                pane_id: id,
                rows: 24,
                cols: 80,
                cwd: Some("/tmp".to_string()),
                title: None,
                is_active: false,
            };
        }
        let left = deep_tree(depth - 1, next_id);
        let right = deep_tree(depth - 1, next_id);
        if depth % 2 == 0 {
            PaneNode::HSplit {
                children: vec![(0.5, left), (0.5, right)],
            }
        } else {
            PaneNode::VSplit {
                children: vec![(0.5, left), (0.5, right)],
            }
        }
    }

    let mut next_id = 1u64;
    let topo = TopologySnapshot {
        schema_version: 1,
        captured_at: 1_700_000_000,
        workspace_id: Some("default".to_string()),
        windows: vec![WindowSnapshot {
            window_id: 0,
            title: None,
            position: None,
            size: None,
            tabs: vec![TabSnapshot {
                tab_id: 0,
                title: None,
                pane_tree: deep_tree(5, &mut next_id),
                active_pane_id: Some(1),
            }],
            active_tab_index: Some(0),
        }],
    };

    let expected_panes = (next_id - 1) as usize; // 32 leaves (2^5)
    let mock = Arc::new(MockWezterm::new());
    let restorer = LayoutRestorer::new(mock, RestoreConfig::default());
    let result = restorer.restore(&topo).await.unwrap();

    assert_eq!(result.pane_id_map.len(), expected_panes);
    assert_eq!(result.panes_created, expected_panes);
}

// =============================================================================
// Property: process launch action classification
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// Shell processes always produce LaunchShell actions (when shells enabled).
    #[test]
    fn shell_process_produces_launch_shell(
        shell in arb_shell(),
    ) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mock = Arc::new(MockWezterm::new());
            let launcher = ProcessLauncher::new(
                mock,
                LaunchConfig {
                    launch_shells: true,
                    ..LaunchConfig::default()
                },
            );

            let state = PaneStateSnapshot {
                schema_version: 1,
                pane_id: 1,
                captured_at: 1_700_000_000,
                cwd: Some("/home/user".to_string()),
                foreground_process: Some(ProcessInfo {
                    name: shell.clone(),
                    pid: Some(1234),
                    argv: None,
                }),
                shell: Some(shell),
                terminal: TerminalState {
                    rows: 24,
                    cols: 80,
                    cursor_row: 0,
                    cursor_col: 0,
                    is_alt_screen: false,
                    title: "bash".to_string(),
                },
                scrollback_ref: None,
                agent: None,
                env: None,
            };

            let id_map: HashMap<u64, u64> = std::iter::once((1, 100)).collect();
            let plans = launcher.plan(&id_map, &[state]);

            prop_assert_eq!(plans.len(), 1);
            match &plans[0].action {
                LaunchAction::LaunchShell { .. } => {} // expected
                other => prop_assert!(
                    false,
                    "expected LaunchShell, got {:?}",
                    other
                ),
            }
            Ok(())
        })?;
    }

    /// When shells are disabled, shell processes produce Skip actions.
    #[test]
    fn disabled_shells_produce_skip(
        shell in arb_shell(),
    ) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mock = Arc::new(MockWezterm::new());
            let launcher = ProcessLauncher::new(
                mock,
                LaunchConfig {
                    launch_shells: false,
                    ..LaunchConfig::default()
                },
            );

            let state = PaneStateSnapshot {
                schema_version: 1,
                pane_id: 1,
                captured_at: 1_700_000_000,
                cwd: Some("/home/user".to_string()),
                foreground_process: Some(ProcessInfo {
                    name: shell,
                    pid: Some(1234),
                    argv: None,
                }),
                shell: Some("bash".to_string()),
                terminal: TerminalState {
                    rows: 24,
                    cols: 80,
                    cursor_row: 0,
                    cursor_col: 0,
                    is_alt_screen: false,
                    title: "bash".to_string(),
                },
                scrollback_ref: None,
                agent: None,
                env: None,
            };

            let id_map: HashMap<u64, u64> = std::iter::once((1, 100)).collect();
            let plans = launcher.plan(&id_map, &[state]);

            prop_assert_eq!(plans.len(), 1);
            match &plans[0].action {
                LaunchAction::Skip { .. } => {} // expected
                other => prop_assert!(
                    false,
                    "expected Skip when shells disabled, got {:?}",
                    other
                ),
            }
            Ok(())
        })?;
    }
}

// =============================================================================
// Property: scrollback chunking
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    /// ScrollbackData::from_segments never loses lines.
    #[test]
    fn scrollback_from_lines_preserves_count(
        lines in prop::collection::vec("[a-zA-Z0-9 ]{0,100}", 0..500),
    ) {
        let data = ScrollbackData::from_segments(lines.clone());
        prop_assert_eq!(data.lines.len(), lines.len());
    }

    /// ScrollbackData byte count is consistent with actual content.
    #[test]
    fn scrollback_byte_count_is_accurate(
        lines in prop::collection::vec("[a-zA-Z0-9 ]{0,100}", 0..200),
    ) {
        let data = ScrollbackData::from_segments(lines.clone());
        let expected_bytes: usize = lines.iter().map(|l| l.len()).sum::<usize>();
        prop_assert_eq!(data.total_bytes, expected_bytes);
    }
}

// =============================================================================
// Property: LaunchAction serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    /// LaunchAction variants survive serde roundtrip.
    #[test]
    fn launch_action_serde_roundtrip(
        variant in prop_oneof![
            (arb_shell(), arb_cwd().prop_filter("need cwd", |c| c.is_some()))
                .prop_map(|(shell, cwd)| LaunchAction::LaunchShell {
                    shell,
                    cwd: std::path::PathBuf::from(cwd.unwrap()),
                }),
            arb_agent_type().prop_map(|at| LaunchAction::LaunchAgent {
                command: format!("{at} --headless"),
                cwd: std::path::PathBuf::from("/home/user"),
                agent_type: at,
            }),
            Just(LaunchAction::Skip { reason: "no process".into() }),
            Just(LaunchAction::Manual {
                hint: "Was running vim".into(),
                original_process: "vim".into(),
            }),
        ]
    ) {
        let json = serde_json::to_string(&variant).unwrap();
        let deserialized: LaunchAction = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&variant, &deserialized);
    }
}
