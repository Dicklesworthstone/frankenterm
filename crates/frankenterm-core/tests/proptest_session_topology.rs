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
//!     19-33: from_panes, match_panes, type tags (existing)
//! 34. LifecycleEntityKind: serde roundtrip + as_str correctness
//! 35. LifecycleEvent: serde roundtrip (all 10 variants)
//! 36. SessionLifecycleState: serde roundtrip
//! 37. WindowLifecycleState: serde roundtrip
//! 38. MuxPaneLifecycleState: serde roundtrip
//! 39. AgentLifecycleState: serde roundtrip
//! 40. LifecycleIdentity: serde roundtrip + stable_key format
//! 41. LifecycleState: serde roundtrip + kind() + label()
//! 42. LifecycleEntityRecord: serde roundtrip + stable_key delegation
//! 43. LifecycleTransitionContext: serde roundtrip + new() constructor
//! 44. LifecycleTransitionRequest: serde roundtrip
//! 45. LifecycleDecision: serde roundtrip
//! 46. LifecycleTransitionLogEntry: serde roundtrip
//! 47. Session transitions: Provisioning→Active via Provisioned
//! 48. Pane transitions: Ready→Running→Ready via StartWork/WorkFinished
//! 49. Agent transitions: Registered→Attached→Detached via Attach/Detach
//! 50. ForceClose: universal terminal transition from any state
//! 51. Idempotent transitions: re-draining Draining is noop
//! 52. Invalid transitions: error for illegal state/event combos
//! 53. apply_lifecycle_state_transition: delegates to correct sub-machine

use proptest::prelude::*;

use frankenterm_core::session_topology::{
    AgentLifecycleState, InferenceQuality, LifecycleDecision, LifecycleEntityKind,
    LifecycleEntityRecord, LifecycleEvent, LifecycleIdentity, LifecycleState,
    LifecycleTransitionContext, LifecycleTransitionLogEntry, LifecycleTransitionRequest,
    MuxPaneLifecycleState, PaneNode, SessionLifecycleState, TOPOLOGY_SCHEMA_VERSION, TabSnapshot,
    TopologySnapshot, WindowLifecycleState, WindowSnapshot, apply_lifecycle_state_transition,
    transition_agent_state, transition_pane_state, transition_session_state,
    transition_window_state,
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

// =============================================================================
// Lifecycle strategies
// =============================================================================

fn arb_lifecycle_entity_kind() -> impl Strategy<Value = LifecycleEntityKind> {
    prop_oneof![
        Just(LifecycleEntityKind::Session),
        Just(LifecycleEntityKind::Window),
        Just(LifecycleEntityKind::Pane),
        Just(LifecycleEntityKind::Agent),
    ]
}

fn arb_lifecycle_event() -> impl Strategy<Value = LifecycleEvent> {
    prop_oneof![
        Just(LifecycleEvent::Provisioned),
        Just(LifecycleEvent::StartWork),
        Just(LifecycleEvent::WorkFinished),
        Just(LifecycleEvent::Attach),
        Just(LifecycleEvent::Detach),
        Just(LifecycleEvent::DrainRequested),
        Just(LifecycleEvent::DrainCompleted),
        Just(LifecycleEvent::PeerDisconnected),
        Just(LifecycleEvent::Recover),
        Just(LifecycleEvent::ForceClose),
    ]
}

fn arb_session_lifecycle_state() -> impl Strategy<Value = SessionLifecycleState> {
    prop_oneof![
        Just(SessionLifecycleState::Provisioning),
        Just(SessionLifecycleState::Active),
        Just(SessionLifecycleState::Draining),
        Just(SessionLifecycleState::Recovering),
        Just(SessionLifecycleState::Closed),
    ]
}

fn arb_window_lifecycle_state() -> impl Strategy<Value = WindowLifecycleState> {
    prop_oneof![
        Just(WindowLifecycleState::Provisioning),
        Just(WindowLifecycleState::Active),
        Just(WindowLifecycleState::Draining),
        Just(WindowLifecycleState::Recovering),
        Just(WindowLifecycleState::Closed),
    ]
}

fn arb_pane_lifecycle_state() -> impl Strategy<Value = MuxPaneLifecycleState> {
    prop_oneof![
        Just(MuxPaneLifecycleState::Provisioning),
        Just(MuxPaneLifecycleState::Ready),
        Just(MuxPaneLifecycleState::Running),
        Just(MuxPaneLifecycleState::Draining),
        Just(MuxPaneLifecycleState::Orphaned),
        Just(MuxPaneLifecycleState::Closed),
    ]
}

fn arb_agent_lifecycle_state() -> impl Strategy<Value = AgentLifecycleState> {
    prop_oneof![
        Just(AgentLifecycleState::Registered),
        Just(AgentLifecycleState::Attached),
        Just(AgentLifecycleState::Detached),
        Just(AgentLifecycleState::Retired),
    ]
}

fn arb_lifecycle_state() -> impl Strategy<Value = LifecycleState> {
    prop_oneof![
        arb_session_lifecycle_state().prop_map(LifecycleState::Session),
        arb_window_lifecycle_state().prop_map(LifecycleState::Window),
        arb_pane_lifecycle_state().prop_map(LifecycleState::Pane),
        arb_agent_lifecycle_state().prop_map(LifecycleState::Agent),
    ]
}

fn arb_lifecycle_identity() -> impl Strategy<Value = LifecycleIdentity> {
    (
        arb_lifecycle_entity_kind(),
        "[a-z_]{3,10}",
        "[a-z_]{3,10}",
        0_u64..10000,
        0_u64..100,
    )
        .prop_map(|(kind, ws, domain, local_id, generation)| {
            LifecycleIdentity::new(kind, ws, domain, local_id, generation)
        })
}

fn arb_lifecycle_decision() -> impl Strategy<Value = LifecycleDecision> {
    prop_oneof![
        Just(LifecycleDecision::Applied),
        Just(LifecycleDecision::Noop),
        Just(LifecycleDecision::Rejected),
    ]
}

// =============================================================================
// Property 34: LifecycleEntityKind serde roundtrip + as_str
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn lifecycle_entity_kind_serde_and_as_str(
        kind in arb_lifecycle_entity_kind(),
    ) {
        let json = serde_json::to_string(&kind).unwrap();
        let back: LifecycleEntityKind = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, kind);

        // as_str matches snake_case serde
        let expected = match kind {
            LifecycleEntityKind::Session => "session",
            LifecycleEntityKind::Window => "window",
            LifecycleEntityKind::Pane => "pane",
            LifecycleEntityKind::Agent => "agent",
        };
        prop_assert_eq!(kind.as_str(), expected);
        // JSON value should be the quoted snake_case string
        let expected_json = format!("\"{}\"", expected);
        prop_assert_eq!(json, expected_json);
    }
}

// =============================================================================
// Property 35: LifecycleEvent serde roundtrip (10 variants)
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn lifecycle_event_serde_roundtrip(
        event in arb_lifecycle_event(),
    ) {
        let json = serde_json::to_string(&event).unwrap();
        let back: LifecycleEvent = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, event);
        // snake_case: verify JSON is a quoted string
        prop_assert!(json.starts_with('"') && json.ends_with('"'));
    }
}

// =============================================================================
// Property 36: SessionLifecycleState serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn session_lifecycle_state_serde(
        state in arb_session_lifecycle_state(),
    ) {
        let json = serde_json::to_string(&state).unwrap();
        let back: SessionLifecycleState = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, state);
    }
}

// =============================================================================
// Property 37: WindowLifecycleState serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn window_lifecycle_state_serde(
        state in arb_window_lifecycle_state(),
    ) {
        let json = serde_json::to_string(&state).unwrap();
        let back: WindowLifecycleState = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, state);
    }
}

// =============================================================================
// Property 38: MuxPaneLifecycleState serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn pane_lifecycle_state_serde(
        state in arb_pane_lifecycle_state(),
    ) {
        let json = serde_json::to_string(&state).unwrap();
        let back: MuxPaneLifecycleState = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, state);
    }
}

// =============================================================================
// Property 39: AgentLifecycleState serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn agent_lifecycle_state_serde(
        state in arb_agent_lifecycle_state(),
    ) {
        let json = serde_json::to_string(&state).unwrap();
        let back: AgentLifecycleState = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, state);
    }
}

// =============================================================================
// Property 40: LifecycleIdentity serde roundtrip + stable_key format
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn lifecycle_identity_serde_and_stable_key(
        identity in arb_lifecycle_identity(),
    ) {
        // Serde roundtrip
        let json = serde_json::to_string(&identity).unwrap();
        let back: LifecycleIdentity = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back, &identity);

        // stable_key format: workspace:domain:kind:local_id:generation
        let key = identity.stable_key();
        let parts: Vec<&str> = key.split(':').collect();
        prop_assert_eq!(parts.len(), 5);
        prop_assert_eq!(parts[0], identity.workspace_id.as_str());
        prop_assert_eq!(parts[1], identity.domain.as_str());
        prop_assert_eq!(parts[2], identity.kind.as_str());
        let local_id_str = format!("{}", identity.local_id);
        prop_assert_eq!(parts[3], local_id_str.as_str());
        let gen_str = format!("{}", identity.generation);
        prop_assert_eq!(parts[4], gen_str.as_str());
    }
}

// =============================================================================
// Property 41: LifecycleState serde roundtrip + kind() + label()
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn lifecycle_state_serde_kind_label(
        state in arb_lifecycle_state(),
    ) {
        // Serde roundtrip
        let json = serde_json::to_string(&state).unwrap();
        let back: LifecycleState = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, state);

        // kind() returns matching entity kind
        let expected_kind = match state {
            LifecycleState::Session(_) => LifecycleEntityKind::Session,
            LifecycleState::Window(_) => LifecycleEntityKind::Window,
            LifecycleState::Pane(_) => LifecycleEntityKind::Pane,
            LifecycleState::Agent(_) => LifecycleEntityKind::Agent,
        };
        prop_assert_eq!(state.kind(), expected_kind);

        // label() returns non-empty string
        let label = state.label();
        prop_assert!(!label.is_empty());

        // tagged serde: JSON contains "kind" and "state" fields
        prop_assert!(json.contains("\"kind\""));
        prop_assert!(json.contains("\"state\""));
    }
}

// =============================================================================
// Property 42: LifecycleEntityRecord serde roundtrip + stable_key
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn lifecycle_entity_record_serde(
        identity in arb_lifecycle_identity(),
        state in arb_lifecycle_state(),
        version in 0_u64..1000,
        updated_at in 0_u64..u64::MAX / 2,
        has_event in proptest::bool::ANY,
        event in arb_lifecycle_event(),
    ) {
        let record = LifecycleEntityRecord {
            identity: identity.clone(),
            state,
            version,
            updated_at_ms: updated_at,
            last_event: if has_event { Some(event) } else { None },
        };
        let json = serde_json::to_string(&record).unwrap();
        let back: LifecycleEntityRecord = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.identity, &record.identity);
        prop_assert_eq!(back.state, record.state);
        prop_assert_eq!(back.version, record.version);
        prop_assert_eq!(back.updated_at_ms, record.updated_at_ms);
        // last_event: None serialized as absent (skip_serializing_if), default on deser
        prop_assert_eq!(back.last_event, record.last_event);

        // stable_key delegates to identity
        prop_assert_eq!(record.stable_key(), identity.stable_key());
    }
}

// =============================================================================
// Property 43: LifecycleTransitionContext serde roundtrip + new()
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn lifecycle_transition_context_serde(
        ts in 0_u64..u64::MAX / 2,
        component in "[a-z_]{3,10}",
        correlation_id in "[a-f0-9]{8}",
        scenario_id in "[a-z_]{3,8}",
        reason_code in "[A-Z_]{3,8}",
    ) {
        let ctx = LifecycleTransitionContext::new(
            ts,
            component.as_str(),
            correlation_id.as_str(),
            scenario_id.as_str(),
            reason_code.as_str(),
        );
        prop_assert_eq!(ctx.timestamp_ms, ts);
        prop_assert_eq!(ctx.component.as_str(), component.as_str());
        prop_assert_eq!(ctx.correlation_id.as_str(), correlation_id.as_str());
        prop_assert_eq!(ctx.scenario_id.as_str(), scenario_id.as_str());
        prop_assert_eq!(ctx.reason_code.as_str(), reason_code.as_str());

        let json = serde_json::to_string(&ctx).unwrap();
        let back: LifecycleTransitionContext = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.timestamp_ms, ctx.timestamp_ms);
        prop_assert_eq!(back.component, ctx.component);
        prop_assert_eq!(back.correlation_id, ctx.correlation_id);
        prop_assert_eq!(back.scenario_id, ctx.scenario_id);
        prop_assert_eq!(back.reason_code, ctx.reason_code);
    }
}

// =============================================================================
// Property 44: LifecycleTransitionRequest serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn lifecycle_transition_request_serde(
        identity in arb_lifecycle_identity(),
        event in arb_lifecycle_event(),
        has_version in proptest::bool::ANY,
        expected_version in 0_u64..1000,
        ts in 0_u64..u64::MAX / 2,
    ) {
        let req = LifecycleTransitionRequest {
            identity: identity.clone(),
            event,
            expected_version: if has_version { Some(expected_version) } else { None },
            context: LifecycleTransitionContext::new(ts, "test", "corr1", "scen1", "CODE"),
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: LifecycleTransitionRequest = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.identity, &req.identity);
        prop_assert_eq!(back.event, req.event);
        prop_assert_eq!(back.expected_version, req.expected_version);
        prop_assert_eq!(back.context.timestamp_ms, req.context.timestamp_ms);
    }
}

// =============================================================================
// Property 45: LifecycleDecision serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn lifecycle_decision_serde(
        decision in arb_lifecycle_decision(),
    ) {
        let json = serde_json::to_string(&decision).unwrap();
        let back: LifecycleDecision = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, decision);
        // snake_case
        prop_assert!(json.starts_with('"') && json.ends_with('"'));
    }
}

// =============================================================================
// Property 46: LifecycleTransitionLogEntry serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn lifecycle_transition_log_entry_serde(
        ts in 0_u64..u64::MAX / 2,
        decision in arb_lifecycle_decision(),
        entity in arb_lifecycle_entity_kind(),
        event in arb_lifecycle_event(),
        actual_version in 0_u64..1000,
        has_expected in proptest::bool::ANY,
        expected_v in 0_u64..1000,
        has_error in proptest::bool::ANY,
    ) {
        let entry = LifecycleTransitionLogEntry {
            timestamp_ms: ts,
            component: "engine".to_string(),
            correlation_id: "abc123".to_string(),
            scenario_id: "s1".to_string(),
            identity_key: "ws:dom:pane:42:1".to_string(),
            entity,
            event,
            input_state: "active".to_string(),
            output_state: "draining".to_string(),
            decision,
            expected_version: if has_expected { Some(expected_v) } else { None },
            actual_version,
            reason_code: "DRAIN".to_string(),
            error_code: if has_error { Some("ERR_01".to_string()) } else { None },
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: LifecycleTransitionLogEntry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.timestamp_ms, entry.timestamp_ms);
        prop_assert_eq!(back.entity, entry.entity);
        prop_assert_eq!(back.event, entry.event);
        prop_assert_eq!(back.decision, entry.decision);
        prop_assert_eq!(back.actual_version, entry.actual_version);
        prop_assert_eq!(back.expected_version, entry.expected_version);
        prop_assert_eq!(back.error_code, entry.error_code);
        prop_assert_eq!(back.identity_key, entry.identity_key);
    }
}

// =============================================================================
// Property 47: Session transitions: Provisioning→Active via Provisioned
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn session_provisioning_to_active(_dummy in 0..1_u32) {
        let result = transition_session_state(
            SessionLifecycleState::Provisioning,
            LifecycleEvent::Provisioned,
        );
        let outcome = result.unwrap();
        prop_assert_eq!(outcome.next_state, SessionLifecycleState::Active);
        prop_assert!(!outcome.idempotent);

        // Active → DrainRequested → Draining
        let result2 = transition_session_state(
            SessionLifecycleState::Active,
            LifecycleEvent::DrainRequested,
        );
        let outcome2 = result2.unwrap();
        prop_assert_eq!(outcome2.next_state, SessionLifecycleState::Draining);

        // Draining → DrainCompleted → Closed
        let result3 = transition_session_state(
            SessionLifecycleState::Draining,
            LifecycleEvent::DrainCompleted,
        );
        let outcome3 = result3.unwrap();
        prop_assert_eq!(outcome3.next_state, SessionLifecycleState::Closed);
    }
}

// =============================================================================
// Property 48: Pane transitions: Ready→Running→Ready
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn pane_work_cycle(_dummy in 0..1_u32) {
        // Provisioning → Ready
        let r1 = transition_pane_state(
            MuxPaneLifecycleState::Provisioning,
            LifecycleEvent::Provisioned,
        ).unwrap();
        prop_assert_eq!(r1.next_state, MuxPaneLifecycleState::Ready);

        // Ready → Running
        let r2 = transition_pane_state(
            MuxPaneLifecycleState::Ready,
            LifecycleEvent::StartWork,
        ).unwrap();
        prop_assert_eq!(r2.next_state, MuxPaneLifecycleState::Running);

        // Running → Ready
        let r3 = transition_pane_state(
            MuxPaneLifecycleState::Running,
            LifecycleEvent::WorkFinished,
        ).unwrap();
        prop_assert_eq!(r3.next_state, MuxPaneLifecycleState::Ready);

        // PeerDisconnected → Orphaned
        let r4 = transition_pane_state(
            MuxPaneLifecycleState::Running,
            LifecycleEvent::PeerDisconnected,
        ).unwrap();
        prop_assert_eq!(r4.next_state, MuxPaneLifecycleState::Orphaned);

        // Orphaned → Ready via Recover
        let r5 = transition_pane_state(
            MuxPaneLifecycleState::Orphaned,
            LifecycleEvent::Recover,
        ).unwrap();
        prop_assert_eq!(r5.next_state, MuxPaneLifecycleState::Ready);
    }
}

// =============================================================================
// Property 49: Agent transitions: Registered→Attached→Detached
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn agent_attach_detach_cycle(_dummy in 0..1_u32) {
        // Registered → Attached
        let r1 = transition_agent_state(
            AgentLifecycleState::Registered,
            LifecycleEvent::Attach,
        ).unwrap();
        prop_assert_eq!(r1.next_state, AgentLifecycleState::Attached);

        // Attached → Detached
        let r2 = transition_agent_state(
            AgentLifecycleState::Attached,
            LifecycleEvent::Detach,
        ).unwrap();
        prop_assert_eq!(r2.next_state, AgentLifecycleState::Detached);

        // Detached → Attached (re-attach)
        let r3 = transition_agent_state(
            AgentLifecycleState::Detached,
            LifecycleEvent::Attach,
        ).unwrap();
        prop_assert_eq!(r3.next_state, AgentLifecycleState::Attached);
    }
}

// =============================================================================
// Property 50: ForceClose is universal terminal transition
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn force_close_from_any_session_state(
        state in arb_session_lifecycle_state(),
    ) {
        let result = transition_session_state(state, LifecycleEvent::ForceClose);
        let outcome = result.unwrap();
        prop_assert_eq!(outcome.next_state, SessionLifecycleState::Closed);
    }

    #[test]
    fn force_close_from_any_window_state(
        state in arb_window_lifecycle_state(),
    ) {
        let result = transition_window_state(state, LifecycleEvent::ForceClose);
        let outcome = result.unwrap();
        prop_assert_eq!(outcome.next_state, WindowLifecycleState::Closed);
    }

    #[test]
    fn force_close_from_any_pane_state(
        state in arb_pane_lifecycle_state(),
    ) {
        let result = transition_pane_state(state, LifecycleEvent::ForceClose);
        let outcome = result.unwrap();
        prop_assert_eq!(outcome.next_state, MuxPaneLifecycleState::Closed);
    }

    #[test]
    fn force_close_from_any_agent_state(
        state in arb_agent_lifecycle_state(),
    ) {
        let result = transition_agent_state(state, LifecycleEvent::ForceClose);
        let outcome = result.unwrap();
        prop_assert_eq!(outcome.next_state, AgentLifecycleState::Retired);
    }
}

// =============================================================================
// Property 51: Idempotent transitions are flagged
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn idempotent_drain_on_draining(_dummy in 0..1_u32) {
        // Session: Draining + DrainRequested → noop(Draining)
        let r1 = transition_session_state(
            SessionLifecycleState::Draining,
            LifecycleEvent::DrainRequested,
        ).unwrap();
        prop_assert_eq!(r1.next_state, SessionLifecycleState::Draining);
        prop_assert!(r1.idempotent);

        // Pane: Draining + DrainRequested → noop(Draining)
        let r2 = transition_pane_state(
            MuxPaneLifecycleState::Draining,
            LifecycleEvent::DrainRequested,
        ).unwrap();
        prop_assert_eq!(r2.next_state, MuxPaneLifecycleState::Draining);
        prop_assert!(r2.idempotent);

        // Agent: Attached + Attach → noop(Attached)
        let r3 = transition_agent_state(
            AgentLifecycleState::Attached,
            LifecycleEvent::Attach,
        ).unwrap();
        prop_assert_eq!(r3.next_state, AgentLifecycleState::Attached);
        prop_assert!(r3.idempotent);

        // Agent: Detached + Detach → noop(Detached)
        let r4 = transition_agent_state(
            AgentLifecycleState::Detached,
            LifecycleEvent::Detach,
        ).unwrap();
        prop_assert_eq!(r4.next_state, AgentLifecycleState::Detached);
        prop_assert!(r4.idempotent);

        // Session: Closed + ForceClose → noop(Closed)
        let r5 = transition_session_state(
            SessionLifecycleState::Closed,
            LifecycleEvent::ForceClose,
        ).unwrap();
        prop_assert_eq!(r5.next_state, SessionLifecycleState::Closed);
        prop_assert!(r5.idempotent);
    }
}

// =============================================================================
// Property 52: Invalid transitions return error
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn invalid_session_transitions(_dummy in 0..1_u32) {
        // Provisioning + StartWork → error
        let r1 = transition_session_state(
            SessionLifecycleState::Provisioning,
            LifecycleEvent::StartWork,
        );
        prop_assert!(r1.is_err());

        // Closed + Provisioned → error
        let r2 = transition_session_state(
            SessionLifecycleState::Closed,
            LifecycleEvent::Provisioned,
        );
        prop_assert!(r2.is_err());

        // Active + WorkFinished → error
        let r3 = transition_session_state(
            SessionLifecycleState::Active,
            LifecycleEvent::WorkFinished,
        );
        prop_assert!(r3.is_err());
    }

    #[test]
    fn invalid_pane_transitions(_dummy in 0..1_u32) {
        // Closed + StartWork → error
        let r1 = transition_pane_state(
            MuxPaneLifecycleState::Closed,
            LifecycleEvent::StartWork,
        );
        prop_assert!(r1.is_err());

        // Provisioning + WorkFinished → error
        let r2 = transition_pane_state(
            MuxPaneLifecycleState::Provisioning,
            LifecycleEvent::WorkFinished,
        );
        prop_assert!(r2.is_err());
    }

    #[test]
    fn invalid_agent_transitions(_dummy in 0..1_u32) {
        // Retired + Attach → error
        let r1 = transition_agent_state(
            AgentLifecycleState::Retired,
            LifecycleEvent::Attach,
        );
        prop_assert!(r1.is_err());

        // Registered + Detach → error
        let r2 = transition_agent_state(
            AgentLifecycleState::Registered,
            LifecycleEvent::Detach,
        );
        prop_assert!(r2.is_err());
    }
}

// =============================================================================
// Property 53: apply_lifecycle_state_transition delegates correctly
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn apply_transition_delegates_force_close(
        state in arb_lifecycle_state(),
    ) {
        let result = apply_lifecycle_state_transition(state, LifecycleEvent::ForceClose);
        let outcome = result.unwrap();
        // ForceClose always succeeds from any state
        let is_terminal = match outcome.next_state {
            LifecycleState::Session(s) => s == SessionLifecycleState::Closed,
            LifecycleState::Window(w) => w == WindowLifecycleState::Closed,
            LifecycleState::Pane(p) => p == MuxPaneLifecycleState::Closed,
            LifecycleState::Agent(a) => a == AgentLifecycleState::Retired,
        };
        prop_assert!(is_terminal);
        // Kind is preserved through delegation
        prop_assert_eq!(outcome.next_state.kind(), state.kind());
    }
}
