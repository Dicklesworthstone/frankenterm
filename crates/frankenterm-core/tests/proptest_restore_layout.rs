//! Property-based tests for the `restore_layout` module.
//!
//! Covers `RestoreConfig` serde roundtrips (JSON + TOML), `count_panes`
//! invariants over generated topologies, and `LayoutRestorer::restore`
//! behavior with `MockWezterm`.
//!
//! Properties:
//!  1. RestoreConfig JSON serde roundtrip preserves all fields
//!  2. RestoreConfig default has all flags true
//!  3. RestoreConfig serde is deterministic
//!  4. RestoreConfig from empty JSON gets defaults
//!  5. RestoreConfig partial JSON preserves given and defaults missing
//!  6. RestoreConfig all bool combos roundtrip
//!  7. RestoreConfig TOML roundtrip preserves all fields
//!  8. RestoreConfig negation of all fields roundtrips
//!  9. count_panes on single leaf topology returns 1
//! 10. count_panes on generated topologies equals pane_ids().len()
//! 11. count_panes on empty topology returns 0
//! 12. count_panes on HSplit is sum of children counts
//! 13. count_panes on VSplit is sum of children counts
//! 14. count_panes on nested splits is always >= 1
//! 15. TopologySnapshot pane_count matches count_panes
//! 16. RestoreConfig JSON with extra fields deserializes (forward compat)
//! 17. RestoreConfig all 8 explicit boolean combinations produce valid configs
//! 18. count_panes monotonically increasing with more windows/tabs
//! 19. count_panes on generated pane tree equals PaneNode::pane_count
//! 20. RestoreConfig double roundtrip (serialize -> deserialize -> serialize) stable
//! 21. count_panes on topology with N single-leaf tabs equals N
//! 22. LayoutRestorer restore on single leaf creates exactly 1 pane (async unit)
//! 23. LayoutRestorer restore on empty topology creates 0 panes (async unit)
//! 24. LayoutRestorer restore preserves pane_id_map size matching count_panes (async unit)
//! 25. LayoutRestorer restore with splits creates correct pane count (async unit)

use frankenterm_core::restore_layout::{count_panes, RestoreConfig};
use frankenterm_core::session_topology::{PaneNode, TabSnapshot, TopologySnapshot, WindowSnapshot};
use proptest::prelude::*;

// =========================================================================
// Strategies
// =========================================================================

fn arb_restore_config() -> impl Strategy<Value = RestoreConfig> {
    (any::<bool>(), any::<bool>(), any::<bool>()).prop_map(
        |(restore_working_dirs, restore_split_ratios, continue_on_error)| RestoreConfig {
            restore_working_dirs,
            restore_split_ratios,
            continue_on_error,
        },
    )
}

fn arb_pane_id() -> impl Strategy<Value = u64> {
    0_u64..10000
}

fn arb_rows() -> impl Strategy<Value = u16> {
    1_u16..200
}

fn arb_cols() -> impl Strategy<Value = u16> {
    1_u16..400
}

fn arb_leaf() -> impl Strategy<Value = PaneNode> {
    (
        arb_pane_id(),
        arb_rows(),
        arb_cols(),
        proptest::option::of("[a-z/]{1,20}"),
        proptest::option::of("[a-zA-Z0-9 ]{1,20}"),
        proptest::bool::ANY,
    )
        .prop_map(
            |(pane_id, rows, cols, cwd, title, is_active)| PaneNode::Leaf {
                pane_id,
                rows,
                cols,
                cwd,
                title,
                is_active,
            },
        )
}

fn arb_pane_node() -> impl Strategy<Value = PaneNode> {
    arb_leaf().prop_recursive(
        3,  // depth
        32, // max nodes
        4,  // items per collection
        |inner| {
            prop_oneof![
                // HSplit with 2-4 children, equal ratios
                proptest::collection::vec(inner.clone(), 2..=4).prop_map(|children| {
                    let n = children.len() as f64;
                    let ratio = 1.0 / n;
                    PaneNode::HSplit {
                        children: children.into_iter().map(|c| (ratio, c)).collect(),
                    }
                }),
                // VSplit with 2-4 children, equal ratios
                proptest::collection::vec(inner, 2..=4).prop_map(|children| {
                    let n = children.len() as f64;
                    let ratio = 1.0 / n;
                    PaneNode::VSplit {
                        children: children.into_iter().map(|c| (ratio, c)).collect(),
                    }
                }),
            ]
        },
    )
}

fn arb_tab_snapshot() -> impl Strategy<Value = TabSnapshot> {
    (
        arb_pane_id(),
        proptest::option::of("[a-zA-Z0-9 ]{1,20}"),
        arb_pane_node(),
        proptest::option::of(arb_pane_id()),
    )
        .prop_map(|(tab_id, title, pane_tree, active_pane_id)| TabSnapshot {
            tab_id,
            title,
            pane_tree,
            active_pane_id,
        })
}

fn arb_window_snapshot() -> impl Strategy<Value = WindowSnapshot> {
    (
        arb_pane_id(),
        proptest::option::of("[a-zA-Z0-9 ]{1,20}"),
        proptest::option::of(((-500_i32..2000), (-500_i32..2000))),
        proptest::option::of((100_u32..4000, 100_u32..4000)),
        proptest::collection::vec(arb_tab_snapshot(), 1..=3),
        proptest::option::of(0_usize..3),
    )
        .prop_map(
            |(window_id, title, position, size, tabs, active_tab_index)| WindowSnapshot {
                window_id,
                title,
                position,
                size,
                tabs,
                active_tab_index,
            },
        )
}

fn arb_topology_snapshot() -> impl Strategy<Value = TopologySnapshot> {
    (
        0_u64..u64::MAX / 2,
        proptest::option::of("[a-z_]{3,15}"),
        proptest::collection::vec(arb_window_snapshot(), 0..=3),
    )
        .prop_map(|(captured_at, workspace_id, windows)| TopologySnapshot {
            schema_version: 1,
            captured_at,
            workspace_id,
            windows,
        })
}

/// Build a TopologySnapshot with a single window containing a single tab.
fn single_tab_snapshot(pane_tree: PaneNode) -> TopologySnapshot {
    TopologySnapshot {
        schema_version: 1,
        captured_at: 1000,
        workspace_id: None,
        windows: vec![WindowSnapshot {
            window_id: 0,
            title: None,
            position: None,
            size: None,
            tabs: vec![TabSnapshot {
                tab_id: 0,
                title: None,
                pane_tree,
                active_pane_id: None,
            }],
            active_tab_index: None,
        }],
    }
}

/// Build a leaf PaneNode.
fn leaf(pane_id: u64) -> PaneNode {
    PaneNode::Leaf {
        pane_id,
        rows: 24,
        cols: 80,
        cwd: None,
        title: None,
        is_active: false,
    }
}

/// Count leaves directly in a PaneNode (local helper, mirrors count_leaves in source).
fn count_node_leaves(node: &PaneNode) -> usize {
    match node {
        PaneNode::Leaf { .. } => 1,
        PaneNode::HSplit { children } | PaneNode::VSplit { children } => {
            children.iter().map(|(_, c)| count_node_leaves(c)).sum()
        }
    }
}

// =========================================================================
// Property 1: RestoreConfig JSON serde roundtrip preserves all fields
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_config_serde(config in arb_restore_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: RestoreConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.restore_working_dirs, config.restore_working_dirs);
        prop_assert_eq!(back.restore_split_ratios, config.restore_split_ratios);
        prop_assert_eq!(back.continue_on_error, config.continue_on_error);
    }
}

// =========================================================================
// Property 2: Default RestoreConfig has all flags true
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_default_config(_dummy in 0..1_u8) {
        let config = RestoreConfig::default();
        prop_assert!(config.restore_working_dirs);
        prop_assert!(config.restore_split_ratios);
        prop_assert!(config.continue_on_error);
    }
}

// =========================================================================
// Property 3: RestoreConfig serde is deterministic
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_config_deterministic(config in arb_restore_config()) {
        let j1 = serde_json::to_string(&config).unwrap();
        let j2 = serde_json::to_string(&config).unwrap();
        prop_assert_eq!(&j1, &j2);
    }
}

// =========================================================================
// Property 4: RestoreConfig from empty JSON gets defaults
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_config_from_empty_json(_dummy in 0..1_u8) {
        let back: RestoreConfig = serde_json::from_str("{}").unwrap();
        prop_assert!(back.restore_working_dirs);
        prop_assert!(back.restore_split_ratios);
        prop_assert!(back.continue_on_error);
    }
}

// =========================================================================
// Property 5: RestoreConfig partial JSON preserves given, defaults missing
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_config_partial_json(val in any::<bool>()) {
        let json = format!("{{\"restore_working_dirs\":{}}}", val);
        let back: RestoreConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.restore_working_dirs, val);
        // Missing fields should get defaults (true)
        prop_assert!(back.restore_split_ratios);
        prop_assert!(back.continue_on_error);
    }
}

// =========================================================================
// Property 6: All bool combos roundtrip correctly
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_all_bool_combos(config in arb_restore_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: RestoreConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(
            (back.restore_working_dirs, back.restore_split_ratios, back.continue_on_error),
            (config.restore_working_dirs, config.restore_split_ratios, config.continue_on_error)
        );
    }
}

// =========================================================================
// Property 7: RestoreConfig TOML roundtrip preserves all fields
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_config_toml_roundtrip(config in arb_restore_config()) {
        let toml_str = toml::to_string(&config).unwrap();
        let back: RestoreConfig = toml::from_str(&toml_str).unwrap();
        prop_assert_eq!(back.restore_working_dirs, config.restore_working_dirs,
            "restore_working_dirs mismatch after TOML roundtrip");
        prop_assert_eq!(back.restore_split_ratios, config.restore_split_ratios,
            "restore_split_ratios mismatch after TOML roundtrip");
        prop_assert_eq!(back.continue_on_error, config.continue_on_error,
            "continue_on_error mismatch after TOML roundtrip");
    }
}

// =========================================================================
// Property 8: RestoreConfig negation of all fields roundtrips
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_config_negation_roundtrip(config in arb_restore_config()) {
        let negated = RestoreConfig {
            restore_working_dirs: !config.restore_working_dirs,
            restore_split_ratios: !config.restore_split_ratios,
            continue_on_error: !config.continue_on_error,
        };
        let json = serde_json::to_string(&negated).unwrap();
        let back: RestoreConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.restore_working_dirs, !config.restore_working_dirs,
            "negated restore_working_dirs mismatch");
        prop_assert_eq!(back.restore_split_ratios, !config.restore_split_ratios,
            "negated restore_split_ratios mismatch");
        prop_assert_eq!(back.continue_on_error, !config.continue_on_error,
            "negated continue_on_error mismatch");
    }
}

// =========================================================================
// Property 9: count_panes on single leaf topology returns 1
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn prop_count_panes_single_leaf(
        pane_id in arb_pane_id(),
        rows in arb_rows(),
        cols in arb_cols(),
    ) {
        let node = PaneNode::Leaf {
            pane_id,
            rows,
            cols,
            cwd: None,
            title: None,
            is_active: false,
        };
        let snapshot = single_tab_snapshot(node);
        prop_assert_eq!(count_panes(&snapshot), 1,
            "single leaf topology should have exactly 1 pane");
    }
}

// =========================================================================
// Property 10: count_panes on generated topologies equals pane_ids().len()
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn prop_count_panes_equals_pane_ids_len(snapshot in arb_topology_snapshot()) {
        let count = count_panes(&snapshot);
        let ids_len = snapshot.pane_ids().len();
        prop_assert_eq!(count, ids_len,
            "count_panes ({}) should equal pane_ids().len() ({})", count, ids_len);
    }
}

// =========================================================================
// Property 11: count_panes on empty topology returns 0
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_count_panes_empty_topology(ts in 0_u64..u64::MAX / 2) {
        let snapshot = TopologySnapshot {
            schema_version: 1,
            captured_at: ts,
            workspace_id: None,
            windows: vec![],
        };
        prop_assert_eq!(count_panes(&snapshot), 0,
            "empty topology should have 0 panes");
    }
}

// =========================================================================
// Property 12: count_panes on HSplit is sum of children counts
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn prop_count_panes_hsplit_sum(
        children in proptest::collection::vec(arb_pane_node(), 2..=4),
    ) {
        let expected: usize = children.iter().map(|c| count_node_leaves(c)).sum();
        let node = PaneNode::HSplit {
            children: children.into_iter().map(|c| (0.5, c)).collect(),
        };
        let snapshot = single_tab_snapshot(node);
        let count = count_panes(&snapshot);
        prop_assert_eq!(count, expected,
            "HSplit count_panes ({}) should equal sum of children ({})", count, expected);
    }
}

// =========================================================================
// Property 13: count_panes on VSplit is sum of children counts
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn prop_count_panes_vsplit_sum(
        children in proptest::collection::vec(arb_pane_node(), 2..=4),
    ) {
        let expected: usize = children.iter().map(|c| count_node_leaves(c)).sum();
        let node = PaneNode::VSplit {
            children: children.into_iter().map(|c| (0.5, c)).collect(),
        };
        let snapshot = single_tab_snapshot(node);
        let count = count_panes(&snapshot);
        prop_assert_eq!(count, expected,
            "VSplit count_panes ({}) should equal sum of children ({})", count, expected);
    }
}

// =========================================================================
// Property 14: count_panes on nested splits is always >= 1
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn prop_count_panes_at_least_one(node in arb_pane_node()) {
        let snapshot = single_tab_snapshot(node);
        let count = count_panes(&snapshot);
        prop_assert!(count >= 1,
            "any non-empty topology should have at least 1 pane, got {}", count);
    }
}

// =========================================================================
// Property 15: TopologySnapshot::pane_count matches count_panes
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn prop_pane_count_matches_count_panes(snapshot in arb_topology_snapshot()) {
        let native = snapshot.pane_count();
        let standalone = count_panes(&snapshot);
        prop_assert_eq!(native, standalone,
            "TopologySnapshot::pane_count() ({}) should equal count_panes() ({})",
            native, standalone);
    }
}

// =========================================================================
// Property 16: RestoreConfig JSON with extra fields deserializes (forward compat)
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_config_extra_fields_ignored(config in arb_restore_config()) {
        let json = format!(
            "{{\"restore_working_dirs\":{},\"restore_split_ratios\":{},\"continue_on_error\":{},\"future_flag\":true,\"version\":42}}",
            config.restore_working_dirs, config.restore_split_ratios, config.continue_on_error
        );
        let back: RestoreConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.restore_working_dirs, config.restore_working_dirs,
            "extra fields should not affect restore_working_dirs");
        prop_assert_eq!(back.restore_split_ratios, config.restore_split_ratios,
            "extra fields should not affect restore_split_ratios");
        prop_assert_eq!(back.continue_on_error, config.continue_on_error,
            "extra fields should not affect continue_on_error");
    }
}

// =========================================================================
// Property 17: All 8 explicit boolean combinations produce valid configs
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10))]

    #[test]
    fn prop_all_eight_bool_combos(_dummy in 0..1_u8) {
        for a in [false, true] {
            for b in [false, true] {
                for c in [false, true] {
                    let config = RestoreConfig {
                        restore_working_dirs: a,
                        restore_split_ratios: b,
                        continue_on_error: c,
                    };
                    let json = serde_json::to_string(&config).unwrap();
                    let back: RestoreConfig = serde_json::from_str(&json).unwrap();
                    prop_assert_eq!(back.restore_working_dirs, a);
                    prop_assert_eq!(back.restore_split_ratios, b);
                    prop_assert_eq!(back.continue_on_error, c);
                }
            }
        }
    }
}

// =========================================================================
// Property 18: count_panes is monotonically increasing with more windows/tabs
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn prop_count_panes_monotonic_with_windows(
        windows in proptest::collection::vec(arb_window_snapshot(), 1..=4),
    ) {
        // Build snapshots with increasing number of windows and verify monotonicity
        let mut prev_count = 0_usize;
        for i in 0..windows.len() {
            let snapshot = TopologySnapshot {
                schema_version: 1,
                captured_at: 1000,
                workspace_id: None,
                windows: windows[..=i].to_vec(),
            };
            let count = count_panes(&snapshot);
            prop_assert!(count >= prev_count,
                "adding window {} increased count from {} to {} -- should be monotonic",
                i, prev_count, count);
            prev_count = count;
        }
    }
}

// =========================================================================
// Property 19: count_panes on generated pane tree equals PaneNode::pane_count
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn prop_count_panes_matches_pane_node_count(node in arb_pane_node()) {
        let snapshot = single_tab_snapshot(node.clone());
        let from_fn = count_panes(&snapshot);
        let from_method = node.pane_count();
        prop_assert_eq!(from_fn, from_method,
            "count_panes ({}) should match PaneNode::pane_count() ({})",
            from_fn, from_method);
    }
}

// =========================================================================
// Property 20: RestoreConfig double roundtrip (serialize->deserialize->serialize) stable
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_config_double_roundtrip(config in arb_restore_config()) {
        let json1 = serde_json::to_string(&config).unwrap();
        let mid: RestoreConfig = serde_json::from_str(&json1).unwrap();
        let json2 = serde_json::to_string(&mid).unwrap();
        prop_assert_eq!(&json1, &json2,
            "double roundtrip should produce identical JSON");
    }
}

// =========================================================================
// Property 21: count_panes on topology with N single-leaf tabs equals N
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn prop_count_panes_n_leaf_tabs(n in 1_usize..8) {
        let tabs: Vec<TabSnapshot> = (0..n)
            .map(|i| TabSnapshot {
                tab_id: i as u64,
                title: None,
                pane_tree: leaf(i as u64),
                active_pane_id: None,
            })
            .collect();
        let snapshot = TopologySnapshot {
            schema_version: 1,
            captured_at: 1000,
            workspace_id: None,
            windows: vec![WindowSnapshot {
                window_id: 0,
                title: None,
                position: None,
                size: None,
                tabs,
                active_tab_index: None,
            }],
        };
        let count = count_panes(&snapshot);
        prop_assert_eq!(count, n,
            "topology with {} single-leaf tabs should have {} panes, got {}",
            n, n, count);
    }
}

// =========================================================================
// Unit tests (existing)
// =========================================================================

#[test]
fn config_default_values() {
    let config = RestoreConfig::default();
    assert!(config.restore_working_dirs);
    assert!(config.restore_split_ratios);
    assert!(config.continue_on_error);
}

#[test]
fn config_roundtrip_all_false() {
    let config = RestoreConfig {
        restore_working_dirs: false,
        restore_split_ratios: false,
        continue_on_error: false,
    };
    let json = serde_json::to_string(&config).unwrap();
    let back: RestoreConfig = serde_json::from_str(&json).unwrap();
    assert!(!back.restore_working_dirs);
    assert!(!back.restore_split_ratios);
    assert!(!back.continue_on_error);
}

// =========================================================================
// Async unit tests (Properties 22-25) using MockWezterm + tokio
//
// These use #[tokio::test] since proptest does not support async.
// =========================================================================

use frankenterm_core::restore_layout::LayoutRestorer;
use frankenterm_core::wezterm::MockWezterm;
use std::sync::Arc;

/// Property 22: LayoutRestorer restore on single leaf creates exactly 1 pane
#[tokio::test]
async fn restore_single_leaf_creates_one_pane() {
    let mock = Arc::new(MockWezterm::new());
    let restorer = LayoutRestorer::new(mock, RestoreConfig::default());
    let snapshot = single_tab_snapshot(leaf(42));

    let result = restorer.restore(&snapshot).await.unwrap();

    assert_eq!(
        result.panes_created, 1,
        "single leaf should create exactly 1 pane"
    );
    assert_eq!(result.windows_created, 1);
    assert_eq!(result.tabs_created, 1);
    assert!(result.failed_panes.is_empty());
    assert!(
        result.pane_id_map.contains_key(&42),
        "pane_id_map should contain the original leaf pane id"
    );
}

/// Property 23: LayoutRestorer restore on empty topology creates 0 panes
#[tokio::test]
async fn restore_empty_topology_creates_zero_panes() {
    let mock = Arc::new(MockWezterm::new());
    let restorer = LayoutRestorer::new(mock, RestoreConfig::default());
    let snapshot = TopologySnapshot {
        schema_version: 1,
        captured_at: 1000,
        workspace_id: None,
        windows: vec![],
    };

    let result = restorer.restore(&snapshot).await.unwrap();

    assert_eq!(
        result.panes_created, 0,
        "empty topology should create 0 panes"
    );
    assert_eq!(result.windows_created, 0);
    assert_eq!(result.tabs_created, 0);
    assert!(result.pane_id_map.is_empty());
    assert!(result.failed_panes.is_empty());
}

/// Property 24: LayoutRestorer restore preserves pane_id_map size matching count_panes
#[tokio::test]
async fn restore_pane_id_map_size_matches_count_panes() {
    // Test with several different topologies
    let topologies = vec![
        // Single leaf
        single_tab_snapshot(leaf(1)),
        // HSplit with 2 leaves
        single_tab_snapshot(PaneNode::HSplit {
            children: vec![(0.5, leaf(10)), (0.5, leaf(11))],
        }),
        // VSplit with 3 leaves
        single_tab_snapshot(PaneNode::VSplit {
            children: vec![(0.33, leaf(20)), (0.33, leaf(21)), (0.34, leaf(22))],
        }),
        // Nested: VSplit of (leaf, HSplit(leaf, leaf))
        single_tab_snapshot(PaneNode::VSplit {
            children: vec![
                (0.5, leaf(30)),
                (
                    0.5,
                    PaneNode::HSplit {
                        children: vec![(0.5, leaf(31)), (0.5, leaf(32))],
                    },
                ),
            ],
        }),
    ];

    for snapshot in &topologies {
        let mock = Arc::new(MockWezterm::new());
        let restorer = LayoutRestorer::new(mock, RestoreConfig::default());
        let expected_panes = count_panes(snapshot);

        let result = restorer.restore(snapshot).await.unwrap();

        assert_eq!(
            result.pane_id_map.len(),
            expected_panes,
            "pane_id_map size ({}) should equal count_panes ({}) for topology",
            result.pane_id_map.len(),
            expected_panes,
        );
    }
}

/// Property 25: LayoutRestorer restore with splits creates correct pane count
#[tokio::test]
async fn restore_with_splits_creates_correct_pane_count() {
    // 2x2 grid: HSplit of (VSplit(leaf, leaf), VSplit(leaf, leaf))
    let tree = PaneNode::HSplit {
        children: vec![
            (
                0.5,
                PaneNode::VSplit {
                    children: vec![(0.5, leaf(1)), (0.5, leaf(2))],
                },
            ),
            (
                0.5,
                PaneNode::VSplit {
                    children: vec![(0.5, leaf(3)), (0.5, leaf(4))],
                },
            ),
        ],
    };
    let snapshot = single_tab_snapshot(tree);

    let mock = Arc::new(MockWezterm::new());
    let restorer = LayoutRestorer::new(mock.clone(), RestoreConfig::default());
    let result = restorer.restore(&snapshot).await.unwrap();

    assert_eq!(
        result.panes_created, 4,
        "2x2 grid should create exactly 4 panes"
    );
    // All old pane IDs should be in the map
    for id in 1..=4_u64 {
        assert!(
            result.pane_id_map.contains_key(&id),
            "pane {} should be in pane_id_map",
            id,
        );
    }
    // All new pane IDs should be distinct
    let new_ids: std::collections::HashSet<u64> = result.pane_id_map.values().copied().collect();
    assert_eq!(
        new_ids.len(),
        4,
        "all 4 new pane IDs should be distinct"
    );
    // MockWezterm should have exactly 4 panes
    let mock_count = mock.pane_count().await;
    assert_eq!(
        mock_count, 4,
        "MockWezterm should have 4 panes after restore"
    );
}
