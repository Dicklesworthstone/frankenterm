//! Property-based tests for session topology invariants.
//!
//! Bead: wa-j0fw
//!
//! Validates:
//! 1. PaneNode::Leaf: pane_count always 1
//! 2. PaneNode::HSplit: pane_count = sum of children
//! 3. PaneNode::VSplit: pane_count = sum of children
//! 4. PaneNode: collect_pane_ids length = pane_count
//! 5. PaneNode: collect_pane_ids contains all leaf IDs
//! 6. PaneNode: serde roundtrip preserves structure
//! 7. PaneNode: split ratios sum to ~1.0 (when constructed properly)
//! 8. TopologySnapshot: serde roundtrip
//! 9. TopologySnapshot::empty: 0 panes, 0 windows
//! 10. TopologySnapshot: pane_count = pane_ids.len()
//! 11. TopologySnapshot: schema_version preserved in roundtrip
//! 12. TopologySnapshot::to_json/from_json roundtrip
//! 13. WindowSnapshot: serde roundtrip
//! 14. TabSnapshot: serde roundtrip
//! 15. InferenceQuality: Inferred != FlatFallback
//! 16. PaneNode recursive: pane_count >= 1
//! 17. TopologySnapshot: captured_at preserved in roundtrip
//! 18. TopologySnapshot: workspace_id preserved in roundtrip

use proptest::prelude::*;

use frankenterm_core::session_topology::{
    InferenceQuality, PaneNode, TOPOLOGY_SCHEMA_VERSION, TabSnapshot, TopologySnapshot,
    WindowSnapshot,
};

// =============================================================================
// Strategies
// =============================================================================

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
            schema_version: TOPOLOGY_SCHEMA_VERSION,
            captured_at,
            workspace_id,
            windows,
        })
}

// =============================================================================
// Property 1: Leaf pane_count always 1
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn leaf_pane_count_is_one(
        pane_id in arb_pane_id(),
        rows in arb_rows(),
        cols in arb_cols(),
    ) {
        let leaf = PaneNode::Leaf { pane_id, rows, cols, cwd: None, title: None, is_active: false };
        prop_assert_eq!(leaf.pane_count(), 1);
    }
}

// =============================================================================
// Property 2: HSplit pane_count = sum of children
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn hsplit_pane_count_is_sum(
        children in proptest::collection::vec(arb_pane_node(), 2..=4),
    ) {
        let expected: usize = children.iter().map(|c| c.pane_count()).sum();
        let node = PaneNode::HSplit {
            children: children.into_iter().map(|c| (0.5, c)).collect(),
        };
        prop_assert_eq!(node.pane_count(), expected);
    }
}

// =============================================================================
// Property 3: VSplit pane_count = sum of children
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn vsplit_pane_count_is_sum(
        children in proptest::collection::vec(arb_pane_node(), 2..=4),
    ) {
        let expected: usize = children.iter().map(|c| c.pane_count()).sum();
        let node = PaneNode::VSplit {
            children: children.into_iter().map(|c| (0.5, c)).collect(),
        };
        prop_assert_eq!(node.pane_count(), expected);
    }
}

// =============================================================================
// Property 4: collect_pane_ids length = pane_count
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn collect_ids_len_equals_pane_count(
        node in arb_pane_node(),
    ) {
        let mut ids = Vec::new();
        node.collect_pane_ids(&mut ids);
        prop_assert_eq!(ids.len(), node.pane_count(),
            "collect_pane_ids gave {} ids but pane_count is {}", ids.len(), node.pane_count());
    }
}

// =============================================================================
// Property 5: collect_pane_ids contains all leaf IDs
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn collect_ids_contains_leaf_ids(
        pane_id in arb_pane_id(),
        rows in arb_rows(),
        cols in arb_cols(),
    ) {
        let leaf = PaneNode::Leaf { pane_id, rows, cols, cwd: None, title: None, is_active: false };
        let mut ids = Vec::new();
        leaf.collect_pane_ids(&mut ids);
        prop_assert_eq!(ids, vec![pane_id]);
    }
}

// =============================================================================
// Property 6: PaneNode serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn pane_node_serde_roundtrip(
        node in arb_pane_node(),
    ) {
        let json = serde_json::to_string(&node).unwrap();
        let back: PaneNode = serde_json::from_str(&json).unwrap();
        // Compare pane counts (f64 ratios may lose precision)
        prop_assert_eq!(back.pane_count(), node.pane_count(),
            "pane_count mismatch after serde roundtrip");
        // Compare pane IDs
        let mut ids_orig = Vec::new();
        let mut ids_back = Vec::new();
        node.collect_pane_ids(&mut ids_orig);
        back.collect_pane_ids(&mut ids_back);
        prop_assert_eq!(ids_orig, ids_back,
            "pane IDs mismatch after serde roundtrip");
    }
}

// =============================================================================
// Property 7: split ratios sum to ~1.0
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn split_ratios_sum_to_one(
        n_children in 2_usize..=6,
    ) {
        let ratio = 1.0 / n_children as f64;
        let children: Vec<(f64, PaneNode)> = (0..n_children)
            .map(|i| (ratio, PaneNode::Leaf {
                pane_id: i as u64,
                rows: 24,
                cols: 80,
                cwd: None,
                title: None,
                is_active: false,
            }))
            .collect();
        let sum: f64 = children.iter().map(|(r, _)| *r).sum();
        prop_assert!((sum - 1.0).abs() < 0.01,
            "ratios should sum to ~1.0, got {}", sum);
    }
}

// =============================================================================
// Property 8: TopologySnapshot serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn topology_serde_roundtrip(
        snapshot in arb_topology_snapshot(),
    ) {
        let json = serde_json::to_string(&snapshot).unwrap();
        let back: TopologySnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.schema_version, snapshot.schema_version);
        prop_assert_eq!(back.captured_at, snapshot.captured_at);
        prop_assert_eq!(back.windows.len(), snapshot.windows.len());
        prop_assert_eq!(back.pane_count(), snapshot.pane_count());
        prop_assert_eq!(back.workspace_id, snapshot.workspace_id);
    }
}

// =============================================================================
// Property 9: TopologySnapshot::empty has 0 panes, 0 windows
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn empty_snapshot_properties(
        ts in 0_u64..u64::MAX / 2,
    ) {
        let snap = TopologySnapshot::empty(ts);
        prop_assert_eq!(snap.pane_count(), 0);
        prop_assert!(snap.windows.is_empty());
        prop_assert!(snap.pane_ids().is_empty());
        prop_assert_eq!(snap.captured_at, ts);
        prop_assert_eq!(snap.schema_version, TOPOLOGY_SCHEMA_VERSION);
    }
}

// =============================================================================
// Property 10: pane_count = pane_ids.len()
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn pane_count_equals_pane_ids_len(
        snapshot in arb_topology_snapshot(),
    ) {
        prop_assert_eq!(snapshot.pane_count(), snapshot.pane_ids().len(),
            "pane_count {} should equal pane_ids.len() {}", snapshot.pane_count(), snapshot.pane_ids().len());
    }
}

// =============================================================================
// Property 11: schema_version preserved in roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn schema_version_preserved(
        version in 1_u32..100,
        ts in 0_u64..u64::MAX / 2,
    ) {
        let snap = TopologySnapshot {
            schema_version: version,
            captured_at: ts,
            workspace_id: None,
            windows: Vec::new(),
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: TopologySnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.schema_version, version);
    }
}

// =============================================================================
// Property 12: to_json/from_json roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn to_from_json_roundtrip(
        snapshot in arb_topology_snapshot(),
    ) {
        let json = snapshot.to_json().unwrap();
        let back = TopologySnapshot::from_json(&json).unwrap();
        prop_assert_eq!(back.schema_version, snapshot.schema_version);
        prop_assert_eq!(back.captured_at, snapshot.captured_at);
        prop_assert_eq!(back.pane_count(), snapshot.pane_count());
        prop_assert_eq!(back.workspace_id, snapshot.workspace_id);
    }
}

// =============================================================================
// Property 13: WindowSnapshot serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn window_snapshot_serde(
        window in arb_window_snapshot(),
    ) {
        let json = serde_json::to_string(&window).unwrap();
        let back: WindowSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.window_id, window.window_id);
        prop_assert_eq!(back.title, window.title);
        prop_assert_eq!(back.position, window.position);
        prop_assert_eq!(back.size, window.size);
        prop_assert_eq!(back.tabs.len(), window.tabs.len());
        prop_assert_eq!(back.active_tab_index, window.active_tab_index);
    }
}

// =============================================================================
// Property 14: TabSnapshot serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn tab_snapshot_serde(
        tab in arb_tab_snapshot(),
    ) {
        let json = serde_json::to_string(&tab).unwrap();
        let back: TabSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.tab_id, tab.tab_id);
        prop_assert_eq!(back.title, tab.title);
        prop_assert_eq!(back.active_pane_id, tab.active_pane_id);
        prop_assert_eq!(back.pane_tree.pane_count(), tab.pane_tree.pane_count());
    }
}

// =============================================================================
// Property 15: InferenceQuality variants are distinct
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10))]

    #[test]
    fn inference_quality_distinct(_dummy in 0..1_u32) {
        prop_assert_ne!(InferenceQuality::Inferred, InferenceQuality::FlatFallback);
        prop_assert_eq!(InferenceQuality::Inferred, InferenceQuality::Inferred);
        prop_assert_eq!(InferenceQuality::FlatFallback, InferenceQuality::FlatFallback);
    }
}

// =============================================================================
// Property 16: PaneNode pane_count >= 1 (recursive)
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn pane_node_count_at_least_one(
        node in arb_pane_node(),
    ) {
        prop_assert!(node.pane_count() >= 1,
            "pane_count should be >= 1, got {}", node.pane_count());
    }
}

// =============================================================================
// Property 17: captured_at preserved in roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn captured_at_preserved(
        ts in 0_u64..u64::MAX / 2,
    ) {
        let snap = TopologySnapshot::empty(ts);
        let json = snap.to_json().unwrap();
        let back = TopologySnapshot::from_json(&json).unwrap();
        prop_assert_eq!(back.captured_at, ts);
    }
}

// =============================================================================
// Property 18: workspace_id preserved in roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn workspace_id_preserved(
        ws in proptest::option::of("[a-z_]{3,15}"),
    ) {
        let snap = TopologySnapshot {
            schema_version: TOPOLOGY_SCHEMA_VERSION,
            captured_at: 12345,
            workspace_id: ws.clone(),
            windows: Vec::new(),
        };
        let json = snap.to_json().unwrap();
        let back = TopologySnapshot::from_json(&json).unwrap();
        prop_assert_eq!(back.workspace_id, ws);
    }
}

// =============================================================================
// Helpers: PaneInfo construction for from_panes / match_panes tests
// =============================================================================

use frankenterm_core::session_topology::match_panes;
use frankenterm_core::wezterm::{PaneInfo, PaneSize};
use std::collections::HashMap;

fn make_pane_info(
    pane_id: u64,
    tab_id: u64,
    window_id: u64,
    rows: u32,
    cols: u32,
    cwd: Option<&str>,
    title: Option<&str>,
    is_active: bool,
) -> PaneInfo {
    PaneInfo {
        pane_id,
        tab_id,
        window_id,
        domain_id: None,
        domain_name: None,
        workspace: None,
        size: Some(PaneSize {
            rows,
            cols,
            pixel_width: None,
            pixel_height: None,
            dpi: None,
        }),
        rows: None,
        cols: None,
        title: title.map(String::from),
        cwd: cwd.map(String::from),
        tty_name: None,
        cursor_x: None,
        cursor_y: None,
        cursor_visibility: None,
        left_col: None,
        top_row: None,
        is_active,
        is_zoomed: false,
        extra: HashMap::new(),
    }
}

// =============================================================================
// Property 19: from_panes — pane_count == input length
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn from_panes_pane_count_equals_input(
        count in 0_usize..8,
    ) {
        let panes: Vec<PaneInfo> = (0..count)
            .map(|i| make_pane_info(i as u64, 0, 0, 24, 80, None, None, i == 0))
            .collect();
        let (snapshot, report) = TopologySnapshot::from_panes(&panes, 1000);
        prop_assert_eq!(snapshot.pane_count(), count,
            "snapshot.pane_count() should equal input count");
        prop_assert_eq!(report.pane_count, count,
            "report.pane_count should equal input count");
    }
}

// =============================================================================
// Property 20: from_panes — report window_count == distinct window_ids
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn from_panes_window_count_matches(
        n_windows in 1_usize..4,
        n_panes_per in 1_usize..3,
    ) {
        let mut panes = Vec::new();
        let mut pane_id = 0u64;
        for w in 0..n_windows {
            for _ in 0..n_panes_per {
                panes.push(make_pane_info(pane_id, 0, w as u64, 24, 80, None, None, pane_id == 0));
                pane_id += 1;
            }
        }
        let (_, report) = TopologySnapshot::from_panes(&panes, 1000);
        prop_assert_eq!(report.window_count, n_windows,
            "report.window_count should equal distinct window IDs");
    }
}

// =============================================================================
// Property 21: from_panes — report tab_count == distinct (window_id, tab_id)
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn from_panes_tab_count_matches(
        n_tabs in 1_usize..5,
    ) {
        let panes: Vec<PaneInfo> = (0..n_tabs)
            .map(|t| make_pane_info(t as u64, t as u64, 0, 24, 80, None, None, t == 0))
            .collect();
        let (_, report) = TopologySnapshot::from_panes(&panes, 1000);
        prop_assert_eq!(report.tab_count, n_tabs,
            "report.tab_count should equal distinct tab IDs");
    }
}

// =============================================================================
// Property 22: from_panes — pane_ids contains all input pane IDs
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn from_panes_pane_ids_complete(
        count in 1_usize..8,
    ) {
        let panes: Vec<PaneInfo> = (0..count)
            .map(|i| make_pane_info(i as u64, 0, 0, 24, 80, None, None, i == 0))
            .collect();
        let (snapshot, _) = TopologySnapshot::from_panes(&panes, 1000);
        let mut ids = snapshot.pane_ids();
        ids.sort_unstable();
        let expected: Vec<u64> = (0..count as u64).collect();
        prop_assert_eq!(ids, expected,
            "pane_ids should contain all input pane IDs");
    }
}

// =============================================================================
// Property 23: from_panes — captured_at is preserved
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn from_panes_captured_at_preserved(
        ts in 0_u64..u64::MAX / 2,
    ) {
        let panes = vec![make_pane_info(0, 0, 0, 24, 80, None, None, true)];
        let (snapshot, _) = TopologySnapshot::from_panes(&panes, ts);
        prop_assert_eq!(snapshot.captured_at, ts);
    }
}

// =============================================================================
// Property 24: from_panes roundtrips through JSON
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn from_panes_json_roundtrip(
        count in 1_usize..5,
    ) {
        let panes: Vec<PaneInfo> = (0..count)
            .map(|i| make_pane_info(i as u64, 0, 0, 24, 80, None, None, i == 0))
            .collect();
        let (snapshot, _) = TopologySnapshot::from_panes(&panes, 1000);
        let json = snapshot.to_json().unwrap();
        let back = TopologySnapshot::from_json(&json).unwrap();
        prop_assert_eq!(back.pane_count(), snapshot.pane_count());
        prop_assert_eq!(back.pane_ids(), snapshot.pane_ids());
    }
}

// =============================================================================
// Property 25: match_panes — mappings + unmatched covers all old pane IDs
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn match_panes_old_coverage(
        n_old in 1_usize..5,
        n_new in 1_usize..5,
    ) {
        let old_panes: Vec<PaneInfo> = (0..n_old)
            .map(|i| make_pane_info(i as u64, 0, 0, 24, 80,
                Some(&format!("/dir{}", i)), Some(&format!("title{}", i)), i == 0))
            .collect();
        let (old_snapshot, _) = TopologySnapshot::from_panes(&old_panes, 1000);

        let new_panes: Vec<PaneInfo> = (0..n_new)
            .map(|i| make_pane_info(100 + i as u64, 0, 0, 24, 80,
                Some(&format!("/dir{}", i)), Some(&format!("title{}", i)), i == 0))
            .collect();

        let mapping = match_panes(&old_snapshot, &new_panes);

        // Every old pane ID is either mapped or unmatched
        let mapped_old: Vec<u64> = mapping.mappings.keys().copied().collect();
        let total = mapped_old.len() + mapping.unmatched_old.len();
        prop_assert_eq!(total, n_old,
            "mapped({}) + unmatched_old({}) should equal n_old({})",
            mapped_old.len(), mapping.unmatched_old.len(), n_old);
    }
}

// =============================================================================
// Property 26: match_panes — mappings + unmatched covers all new pane IDs
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn match_panes_new_coverage(
        n_old in 1_usize..5,
        n_new in 1_usize..5,
    ) {
        let old_panes: Vec<PaneInfo> = (0..n_old)
            .map(|i| make_pane_info(i as u64, 0, 0, 24, 80,
                Some(&format!("/dir{}", i)), Some(&format!("title{}", i)), i == 0))
            .collect();
        let (old_snapshot, _) = TopologySnapshot::from_panes(&old_panes, 1000);

        let new_panes: Vec<PaneInfo> = (0..n_new)
            .map(|i| make_pane_info(100 + i as u64, 0, 0, 24, 80,
                Some(&format!("/dir{}", i)), Some(&format!("title{}", i)), i == 0))
            .collect();

        let mapping = match_panes(&old_snapshot, &new_panes);

        // Every new pane ID is either in mappings values or unmatched_new
        let mapped_new_count = mapping.mappings.values().count();
        let total = mapped_new_count + mapping.unmatched_new.len();
        prop_assert_eq!(total, n_new,
            "mapped_new({}) + unmatched_new({}) should equal n_new({})",
            mapped_new_count, mapping.unmatched_new.len(), n_new);
    }
}

// =============================================================================
// Property 27: match_panes — mappings are injective (no two old map to same new)
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn match_panes_injective(
        n in 1_usize..6,
    ) {
        let old_panes: Vec<PaneInfo> = (0..n)
            .map(|i| make_pane_info(i as u64, 0, 0, 24, 80,
                Some(&format!("/dir{}", i)), None, i == 0))
            .collect();
        let (old_snapshot, _) = TopologySnapshot::from_panes(&old_panes, 1000);

        let new_panes: Vec<PaneInfo> = (0..n)
            .map(|i| make_pane_info(100 + i as u64, 0, 0, 24, 80,
                Some(&format!("/dir{}", i)), None, i == 0))
            .collect();

        let mapping = match_panes(&old_snapshot, &new_panes);

        // No duplicate values in mappings
        let mut seen_new: Vec<u64> = mapping.mappings.values().copied().collect();
        seen_new.sort_unstable();
        seen_new.dedup();
        prop_assert_eq!(seen_new.len(), mapping.mappings.len(),
            "mappings should be injective (no duplicate new pane IDs)");
    }
}

// =============================================================================
// Property 28: match_panes — empty old → all new unmatched
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn match_panes_empty_old(
        n_new in 1_usize..5,
    ) {
        let (old_snapshot, _) = TopologySnapshot::from_panes(&[], 1000);
        let new_panes: Vec<PaneInfo> = (0..n_new)
            .map(|i| make_pane_info(i as u64, 0, 0, 24, 80, None, None, i == 0))
            .collect();

        let mapping = match_panes(&old_snapshot, &new_panes);
        prop_assert!(mapping.mappings.is_empty(),
            "empty old snapshot should have no mappings");
        prop_assert!(mapping.unmatched_old.is_empty());
        prop_assert_eq!(mapping.unmatched_new.len(), n_new);
    }
}

// =============================================================================
// Property 29: match_panes — empty new → all old unmatched
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn match_panes_empty_new(
        n_old in 1_usize..5,
    ) {
        let old_panes: Vec<PaneInfo> = (0..n_old)
            .map(|i| make_pane_info(i as u64, 0, 0, 24, 80,
                Some(&format!("/d{}", i)), None, i == 0))
            .collect();
        let (old_snapshot, _) = TopologySnapshot::from_panes(&old_panes, 1000);

        let mapping = match_panes(&old_snapshot, &[]);
        prop_assert!(mapping.mappings.is_empty(),
            "empty new panes should have no mappings");
        prop_assert_eq!(mapping.unmatched_old.len(), n_old);
        prop_assert!(mapping.unmatched_new.is_empty());
    }
}

// =============================================================================
// Property 30: CaptureReport — inference_quality has entry for each tab
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn capture_report_inference_per_tab(
        n_tabs in 1_usize..5,
    ) {
        let panes: Vec<PaneInfo> = (0..n_tabs)
            .map(|t| make_pane_info(t as u64, t as u64, 0, 24, 80, None, None, t == 0))
            .collect();
        let (_, report) = TopologySnapshot::from_panes(&panes, 1000);
        prop_assert_eq!(report.inference_quality.len(), n_tabs,
            "inference_quality should have entry for each tab");
    }
}

// =============================================================================
// Property 31: TopologySnapshot serde is deterministic
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn topology_serde_deterministic(
        snapshot in arb_topology_snapshot(),
    ) {
        let j1 = serde_json::to_string(&snapshot).unwrap();
        let j2 = serde_json::to_string(&snapshot).unwrap();
        prop_assert_eq!(j1.as_str(), j2.as_str(), "serde should be deterministic");
    }
}

// =============================================================================
// Property 32: PaneNode JSON always contains "type" tag
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn pane_node_json_has_type_tag(
        node in arb_pane_node(),
    ) {
        let json = serde_json::to_string(&node).unwrap();
        prop_assert!(json.contains("\"type\""),
            "PaneNode JSON should contain type tag, got: {}", json);
    }
}

// =============================================================================
// Property 33: PaneNode type tag matches variant
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn pane_node_type_tag_correct(
        node in arb_pane_node(),
    ) {
        let json = serde_json::to_string(&node).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let type_tag = value.get("type").unwrap().as_str().unwrap();
        match &node {
            PaneNode::Leaf { .. } => prop_assert_eq!(type_tag, "Leaf"),
            PaneNode::HSplit { .. } => prop_assert_eq!(type_tag, "HSplit"),
            PaneNode::VSplit { .. } => prop_assert_eq!(type_tag, "VSplit"),
        }
    }
}
