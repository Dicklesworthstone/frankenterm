//! LabRuntime-ported layout restoration tests for deterministic async testing.
//!
//! Ports `#[tokio::test]` functions from `restore_layout.rs` to asupersync-based
//! `RuntimeFixture`, gaining seed-based reproducibility for window/tab/pane
//! topology restoration tests.
//!
//! Bead: ft-22x4r (Port existing async tests to LabRuntime)

#![cfg(feature = "asupersync-runtime")]

mod common;

use common::fixtures::RuntimeFixture;
use frankenterm_core::restore_layout::{LayoutRestorer, RestoreConfig};
use frankenterm_core::session_topology::{PaneNode, TabSnapshot, TopologySnapshot, WindowSnapshot};
use frankenterm_core::wezterm::{MockWezterm, WeztermInterface};
use std::sync::Arc;

// ===========================================================================
// Helpers (mirrors test-private helpers from restore_layout.rs)
// ===========================================================================

fn make_restorer(mock: Arc<MockWezterm>) -> LayoutRestorer {
    LayoutRestorer::new(mock, RestoreConfig::default())
}

fn leaf(pane_id: u64, cwd: Option<&str>) -> PaneNode {
    PaneNode::Leaf {
        pane_id,
        rows: 24,
        cols: 80,
        cwd: cwd.map(String::from),
        title: None,
        is_active: false,
    }
}

fn active_leaf(pane_id: u64) -> PaneNode {
    PaneNode::Leaf {
        pane_id,
        rows: 24,
        cols: 80,
        cwd: None,
        title: None,
        is_active: true,
    }
}

fn hsplit(children: Vec<(f64, PaneNode)>) -> PaneNode {
    PaneNode::HSplit { children }
}

fn vsplit(children: Vec<(f64, PaneNode)>) -> PaneNode {
    PaneNode::VSplit { children }
}

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

/// Reimplemented from private `collect_leaf_ids` in restore_layout.rs.
fn collect_leaf_ids(node: &PaneNode) -> Vec<u64> {
    let mut ids = Vec::new();
    collect_leaf_ids_inner(node, &mut ids);
    ids
}

fn collect_leaf_ids_inner(node: &PaneNode, ids: &mut Vec<u64>) {
    match node {
        PaneNode::Leaf { pane_id, .. } => ids.push(*pane_id),
        PaneNode::HSplit { children } | PaneNode::VSplit { children } => {
            for (_, child) in children {
                collect_leaf_ids_inner(child, ids);
            }
        }
    }
}

// ===========================================================================
// Section 1: Single pane and basic splits
// ===========================================================================

#[test]
fn restore_single_pane() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mock = Arc::new(MockWezterm::new());
        let restorer = make_restorer(mock.clone());
        let snapshot = single_tab_snapshot(leaf(42, Some("/home/user")));

        let result = restorer.restore(&snapshot).await.unwrap();

        assert_eq!(result.panes_created, 1);
        assert_eq!(result.windows_created, 1);
        assert_eq!(result.tabs_created, 1);
        assert!(result.failed_panes.is_empty());
        assert!(result.pane_id_map.contains_key(&42));
    });
}

#[test]
fn restore_horizontal_split() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mock = Arc::new(MockWezterm::new());
        let restorer = make_restorer(mock.clone());
        let tree = hsplit(vec![(0.5, leaf(1, None)), (0.5, leaf(2, None))]);
        let snapshot = single_tab_snapshot(tree);

        let result = restorer.restore(&snapshot).await.unwrap();

        assert_eq!(result.panes_created, 2);
        assert!(result.pane_id_map.contains_key(&1));
        assert!(result.pane_id_map.contains_key(&2));
        assert_ne!(result.pane_id_map[&1], result.pane_id_map[&2]);
    });
}

#[test]
fn restore_vertical_split() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mock = Arc::new(MockWezterm::new());
        let restorer = make_restorer(mock.clone());
        let tree = vsplit(vec![(0.5, leaf(10, None)), (0.5, leaf(11, None))]);
        let snapshot = single_tab_snapshot(tree);

        let result = restorer.restore(&snapshot).await.unwrap();

        assert_eq!(result.panes_created, 2);
        assert!(result.pane_id_map.contains_key(&10));
        assert!(result.pane_id_map.contains_key(&11));
    });
}

// ===========================================================================
// Section 2: Complex topologies
// ===========================================================================

#[test]
fn restore_three_pane_l_shape() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mock = Arc::new(MockWezterm::new());
        let restorer = make_restorer(mock.clone());
        let tree = vsplit(vec![
            (0.5, leaf(1, None)),
            (
                0.5,
                hsplit(vec![(0.5, leaf(2, None)), (0.5, leaf(3, None))]),
            ),
        ]);
        let snapshot = single_tab_snapshot(tree);

        let result = restorer.restore(&snapshot).await.unwrap();

        assert_eq!(result.panes_created, 3);
        for id in [1, 2, 3] {
            assert!(result.pane_id_map.contains_key(&id));
        }
    });
}

#[test]
fn restore_four_pane_grid() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mock = Arc::new(MockWezterm::new());
        let restorer = make_restorer(mock.clone());
        let tree = hsplit(vec![
            (
                0.5,
                vsplit(vec![(0.5, leaf(1, None)), (0.5, leaf(2, None))]),
            ),
            (
                0.5,
                vsplit(vec![(0.5, leaf(3, None)), (0.5, leaf(4, None))]),
            ),
        ]);
        let snapshot = single_tab_snapshot(tree);

        let result = restorer.restore(&snapshot).await.unwrap();

        assert_eq!(result.panes_created, 4);
        for id in 1..=4 {
            assert!(result.pane_id_map.contains_key(&id));
        }
        let new_ids: std::collections::HashSet<_> = result.pane_id_map.values().collect();
        assert_eq!(new_ids.len(), 4);
    });
}

#[test]
fn restore_deeply_nested_splits() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mock = Arc::new(MockWezterm::new());
        let restorer = make_restorer(mock.clone());
        let tree = hsplit(vec![
            (0.5, leaf(1, None)),
            (
                0.5,
                vsplit(vec![
                    (0.5, leaf(2, None)),
                    (
                        0.5,
                        hsplit(vec![
                            (0.5, leaf(3, None)),
                            (
                                0.5,
                                vsplit(vec![
                                    (0.5, leaf(4, None)),
                                    (
                                        0.5,
                                        hsplit(vec![(0.5, leaf(5, None)), (0.5, leaf(6, None))]),
                                    ),
                                ]),
                            ),
                        ]),
                    ),
                ]),
            ),
        ]);
        let snapshot = single_tab_snapshot(tree);

        let result = restorer.restore(&snapshot).await.unwrap();

        assert_eq!(result.panes_created, 6);
        for id in 1..=6 {
            assert!(result.pane_id_map.contains_key(&id));
        }
    });
}

#[test]
fn restore_three_way_split() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mock = Arc::new(MockWezterm::new());
        let restorer = make_restorer(mock.clone());
        let tree = vsplit(vec![
            (0.33, leaf(1, None)),
            (0.33, leaf(2, None)),
            (0.34, leaf(3, None)),
        ]);
        let snapshot = single_tab_snapshot(tree);

        let result = restorer.restore(&snapshot).await.unwrap();

        assert_eq!(result.panes_created, 3);
        let new_ids: std::collections::HashSet<_> = result.pane_id_map.values().collect();
        assert_eq!(new_ids.len(), 3);
    });
}

// ===========================================================================
// Section 3: Multiple tabs and windows
// ===========================================================================

#[test]
fn restore_multiple_tabs() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mock = Arc::new(MockWezterm::new());
        let restorer = make_restorer(mock.clone());
        let snapshot = TopologySnapshot {
            schema_version: 1,
            captured_at: 1000,
            workspace_id: None,
            windows: vec![WindowSnapshot {
                window_id: 0,
                title: None,
                position: None,
                size: None,
                tabs: vec![
                    TabSnapshot {
                        tab_id: 0,
                        title: None,
                        pane_tree: leaf(1, None),
                        active_pane_id: None,
                    },
                    TabSnapshot {
                        tab_id: 1,
                        title: None,
                        pane_tree: vsplit(vec![(0.5, leaf(2, None)), (0.5, leaf(3, None))]),
                        active_pane_id: Some(3),
                    },
                ],
                active_tab_index: Some(1),
            }],
        };

        let result = restorer.restore(&snapshot).await.unwrap();

        assert_eq!(result.tabs_created, 2);
        assert_eq!(result.panes_created, 3);
        for id in 1..=3 {
            assert!(result.pane_id_map.contains_key(&id));
        }
    });
}

#[test]
fn restore_multiple_windows() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mock = Arc::new(MockWezterm::new());
        let restorer = make_restorer(mock.clone());
        let snapshot = TopologySnapshot {
            schema_version: 1,
            captured_at: 1000,
            workspace_id: None,
            windows: vec![
                WindowSnapshot {
                    window_id: 0,
                    title: None,
                    position: None,
                    size: None,
                    tabs: vec![TabSnapshot {
                        tab_id: 0,
                        title: None,
                        pane_tree: leaf(1, None),
                        active_pane_id: None,
                    }],
                    active_tab_index: None,
                },
                WindowSnapshot {
                    window_id: 1,
                    title: None,
                    position: None,
                    size: None,
                    tabs: vec![TabSnapshot {
                        tab_id: 1,
                        title: None,
                        pane_tree: leaf(2, None),
                        active_pane_id: None,
                    }],
                    active_tab_index: None,
                },
            ],
        };

        let result = restorer.restore(&snapshot).await.unwrap();

        assert_eq!(result.windows_created, 2);
        assert_eq!(result.panes_created, 2);
    });
}

// ===========================================================================
// Section 4: Active pane, empty snapshot, pane ID map
// ===========================================================================

#[test]
fn restore_activates_correct_pane() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mock = Arc::new(MockWezterm::new());
        let restorer = make_restorer(mock.clone());
        let snapshot = TopologySnapshot {
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
                    pane_tree: vsplit(vec![(0.5, leaf(1, None)), (0.5, active_leaf(2))]),
                    active_pane_id: Some(2),
                }],
                active_tab_index: None,
            }],
        };

        let result = restorer.restore(&snapshot).await.unwrap();

        assert_eq!(result.panes_created, 2);
        assert!(result.pane_id_map.contains_key(&2));
    });
}

#[test]
fn restore_empty_snapshot() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mock = Arc::new(MockWezterm::new());
        let restorer = make_restorer(mock.clone());
        let snapshot = TopologySnapshot {
            schema_version: 1,
            captured_at: 1000,
            workspace_id: None,
            windows: vec![],
        };

        let result = restorer.restore(&snapshot).await.unwrap();

        assert_eq!(result.windows_created, 0);
        assert_eq!(result.tabs_created, 0);
        assert_eq!(result.panes_created, 0);
    });
}

#[test]
fn pane_id_map_completeness() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mock = Arc::new(MockWezterm::new());
        let restorer = make_restorer(mock.clone());
        let tree = hsplit(vec![
            (0.25, leaf(100, None)),
            (0.25, leaf(200, None)),
            (0.25, leaf(300, None)),
            (0.25, leaf(400, None)),
        ]);
        let snapshot = single_tab_snapshot(tree);

        let result = restorer.restore(&snapshot).await.unwrap();

        let all_leaves = collect_leaf_ids(&snapshot.windows[0].tabs[0].pane_tree);
        for id in &all_leaves {
            assert!(
                result.pane_id_map.contains_key(id),
                "pane {id} missing from pane_id_map"
            );
        }
        assert_eq!(result.pane_id_map.len(), all_leaves.len());
    });
}

// ===========================================================================
// Section 5: CWD restoration and config
// ===========================================================================

#[test]
fn restore_sets_cwd_from_file_uri() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mock = Arc::new(MockWezterm::new());
        let restorer = make_restorer(mock.clone());
        let tree = leaf(1, Some("file:///home/agent/project"));
        let snapshot = single_tab_snapshot(tree);

        let result = restorer.restore(&snapshot).await.unwrap();

        assert_eq!(result.panes_created, 1);
        let new_id = result.pane_id_map[&1];
        let text: String = WeztermInterface::get_text(&*mock, new_id, false)
            .await
            .unwrap();
        assert!(text.contains("/home/agent/project"), "cwd should be set");
    });
}

#[test]
fn config_skip_cwd_restore() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mock = Arc::new(MockWezterm::new());
        let config = RestoreConfig {
            restore_working_dirs: false,
            ..Default::default()
        };
        let restorer = LayoutRestorer::new(mock.clone(), config);
        let tree = leaf(1, Some("/home/user"));
        let snapshot = single_tab_snapshot(tree);

        let result = restorer.restore(&snapshot).await.unwrap();

        assert_eq!(result.panes_created, 1);
        let new_id = result.pane_id_map[&1];
        let text: String = WeztermInterface::get_text(&*mock, new_id, false)
            .await
            .unwrap();
        assert!(!text.contains("cd "), "cwd should not be set");
    });
}
