//! Property tests for [`causal_graph_console`] (ft-1650n.16).
//!
//! Pins the renderer's documented invariants over arbitrary
//! `(nodes, edges, focus_options)` inputs. Complements the
//! substrate's unit tests (19 cases including the linter-extended
//! `render_operator_text` surface).
//!
//! Properties pinned here:
//!
//! 1. **Timeline ordering** — the timeline rendering is sorted
//!    ascending by `(timestamp_ms, node_id)`.
//! 2. **Focus filter conservation** — every node in the
//!    rendered output matches the FocusOptions filter.
//! 3. **Pure rendering** — same input → same output (text +
//!    structured TOON).
//! 4. **Dependency-tree depth bound** — no node appears at a
//!    depth greater than `max_depth + 1` levels deep.
//! 5. **Dependency-tree cycle safety** — a graph with a cycle
//!    does not infinite-loop; each node appears at most once
//!    in the output.
//! 6. **UncertaintyMarker classification thresholds** —
//!    confidence_bps ≥ 8000 ⇒ Confident; threshold ≤ x < 8000
//!    ⇒ LowConfidence; x < threshold ⇒ Uncertain.
//! 7. **Render output non-empty for non-empty input** — the
//!    text renderer always emits the header + at least one
//!    entry when the focus matches.
//! 8. **Schema version stability** — every TOON rendering
//!    carries the documented `CAUSAL_CONSOLE_RENDERING_SCHEMA`.
//! 9. **TOON timeline length matches filtered nodes** —
//!    `rendering.timeline.len() == count(filtered nodes)`.
//! 10. **Uncertain edges subset = confidence_bps < 8000** —
//!     every edge with confidence < 8000 lands in
//!     `uncertain_edges`; all edges with ≥ 8000 do not.

use std::sync::Once;

use frankenterm_core::causal_graph_console::{
    CAUSAL_CONSOLE_RENDERING_SCHEMA, FocusOptions, UncertaintyMarker, render_dependency_tree,
    render_robot_toon, render_timeline_text,
};
use frankenterm_core::explainability_console::{
    CausalEvidenceKind, CausalGraphEdge, CausalGraphNode, CausalNodeKind, CausalTraversalDirection,
};
use proptest::prelude::*;

fn init_test_tracing_json() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .json()
            .with_target(true)
            .with_test_writer()
            .try_init();
    });
}

fn arb_node_kind() -> impl Strategy<Value = CausalNodeKind> {
    prop_oneof![
        Just(CausalNodeKind::PaneEvent),
        Just(CausalNodeKind::PatternMatch),
        Just(CausalNodeKind::WorkflowTrigger),
        Just(CausalNodeKind::MissionDispatch),
        Just(CausalNodeKind::PolicyDecision),
        Just(CausalNodeKind::Approval),
        Just(CausalNodeKind::StorageWrite),
        Just(CausalNodeKind::RecoveryAction),
        Just(CausalNodeKind::UserAction),
    ]
}

fn arb_node(idx: usize) -> impl Strategy<Value = CausalGraphNode> {
    let id = format!("n{idx}");
    (
        arb_node_kind(),
        0u64..=200,
        prop::option::of(0u64..=10),
        "[a-z]{1,8}",
    )
        .prop_map(move |(kind, ts, pane, label)| {
            let mut node = CausalGraphNode::new(id.clone(), kind, ts, label);
            if let Some(p) = pane {
                node = node.with_pane_id(p);
            }
            node
        })
}

fn arb_node_set() -> impl Strategy<Value = Vec<CausalGraphNode>> {
    (1usize..=8).prop_flat_map(|n| (0..n).map(arb_node).collect::<Vec<_>>())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// **Pure rendering**: same input → same output.
    #[test]
    fn render_timeline_text_is_pure(
        nodes in arb_node_set(),
    ) {
        init_test_tracing_json();
        let opts = FocusOptions::default();
        let t1 = render_timeline_text(&nodes, &[], &opts);
        let t2 = render_timeline_text(&nodes, &[], &opts);
        prop_assert_eq!(t1, t2);
    }

    /// **Pure TOON rendering**.
    #[test]
    fn render_robot_toon_is_pure(
        nodes in arb_node_set(),
    ) {
        init_test_tracing_json();
        let opts = FocusOptions::default();
        let r1 = render_robot_toon(&nodes, &[], &opts);
        let r2 = render_robot_toon(&nodes, &[], &opts);
        prop_assert_eq!(r1, r2);
    }

    /// **Schema version stability**.
    #[test]
    fn toon_rendering_carries_schema_version(
        nodes in arb_node_set(),
    ) {
        init_test_tracing_json();
        let r = render_robot_toon(&nodes, &[], &FocusOptions::default());
        prop_assert_eq!(r.schema_version, CAUSAL_CONSOLE_RENDERING_SCHEMA);
    }

    /// **TOON timeline length matches filtered node count**.
    #[test]
    fn toon_timeline_length_matches_filtered_nodes(
        nodes in arb_node_set(),
        pane_filter in prop::option::of(0u64..=10),
    ) {
        init_test_tracing_json();
        let opts = FocusOptions {
            pane_id: pane_filter,
            ..Default::default()
        };
        let expected = if let Some(p) = pane_filter {
            nodes.iter().filter(|n| n.pane_id == Some(p)).count()
        } else {
            nodes.len()
        };
        let r = render_robot_toon(&nodes, &[], &opts);
        prop_assert_eq!(r.timeline.len(), expected);
    }

    /// **Timeline ordering**: timeline entries are sorted
    /// ascending by `(timestamp_ms, node_id)`.
    #[test]
    fn toon_timeline_is_sorted_by_timestamp_and_id(
        nodes in arb_node_set(),
    ) {
        init_test_tracing_json();
        let r = render_robot_toon(&nodes, &[], &FocusOptions::default());
        for w in r.timeline.windows(2) {
            let prev_key = (w[0].timestamp_ms, w[0].node_id.clone());
            let curr_key = (w[1].timestamp_ms, w[1].node_id.clone());
            prop_assert!(
                prev_key <= curr_key,
                "timeline ordering violated: {prev_key:?} !<= {curr_key:?}"
            );
        }
    }

    /// **Focus filter conservation**: every timeline entry
    /// matches the documented FocusOptions filter (pane,
    /// time range, kind subset).
    #[test]
    fn focus_filter_conserved_in_timeline(
        nodes in arb_node_set(),
        pane_filter in prop::option::of(0u64..=10),
        start in prop::option::of(0u64..=200),
        end in prop::option::of(0u64..=200),
    ) {
        init_test_tracing_json();
        let opts = FocusOptions {
            pane_id: pane_filter,
            start_ms: start,
            end_ms: end,
            ..Default::default()
        };
        let r = render_robot_toon(&nodes, &[], &opts);
        for entry in &r.timeline {
            if let Some(p) = pane_filter {
                prop_assert_eq!(entry.pane_id, Some(p));
            }
            if let Some(s) = start {
                prop_assert!(entry.timestamp_ms >= s);
            }
            if let Some(e) = end {
                prop_assert!(entry.timestamp_ms <= e);
            }
        }
    }

    /// **Uncertain edges subset = confidence_bps < 8000**:
    /// every edge with confidence < 8000 lands in
    /// `uncertain_edges`; edges with ≥ 8000 do not.
    #[test]
    fn uncertain_edges_set_matches_confidence_threshold(
        nodes in arb_node_set(),
        edge_count in 1usize..=8,
    ) {
        init_test_tracing_json();
        // Use the node count to bound edge endpoints.
        let node_count = nodes.len();
        let edges: Vec<CausalGraphEdge> = (0..edge_count)
            .map(|i| {
                let from_idx = i % node_count;
                let to_idx = (i + 1) % node_count;
                let conf = (i as u16 * 1500) % 10_001;
                CausalGraphEdge::new(
                    format!("n{from_idx}"),
                    format!("n{to_idx}"),
                    CausalEvidenceKind::Observed,
                    conf,
                    "test",
                )
            })
            .collect();
        let r = render_robot_toon(&nodes, &edges, &FocusOptions::default());
        let expected_count = edges.iter().filter(|e| e.confidence_bps < 8_000).count();
        prop_assert_eq!(r.uncertain_edges.len(), expected_count);
        for ue in &r.uncertain_edges {
            prop_assert!(ue.confidence_bps < 8_000);
        }
    }

    /// **Dependency-tree depth bound**: no node appears more
    /// than `max_depth + 1` levels deep in the descendants
    /// rendering (depth 0 = focus node, max_depth = leaves).
    #[test]
    fn dependency_tree_respects_max_depth(
        nodes in arb_node_set(),
        max_depth in 0u32..=5,
    ) {
        init_test_tracing_json();
        let node_count = nodes.len();
        let edges: Vec<CausalGraphEdge> = (0..node_count.saturating_sub(1))
            .map(|i| {
                CausalGraphEdge::new(
                    format!("n{i}"),
                    format!("n{}", i + 1),
                    CausalEvidenceKind::Observed,
                    9_000,
                    "test",
                )
            })
            .collect();
        let text = render_dependency_tree(
            &nodes,
            &edges,
            "n0",
            CausalTraversalDirection::Descendants,
            max_depth,
            5_000,
        );
        // Each level contributes a leading "  " (2 spaces) per
        // depth level. Count the maximum indentation observed.
        let max_observed_indent = text
            .lines()
            .filter(|l| l.starts_with(' '))
            .map(|l| l.chars().take_while(|c| *c == ' ').count())
            .max()
            .unwrap_or(0);
        let max_allowed_indent = (max_depth as usize) * 2;
        prop_assert!(
            max_observed_indent <= max_allowed_indent,
            "indentation {max_observed_indent} exceeded depth {max_depth} budget {max_allowed_indent}"
        );
    }

    /// **Dependency-tree cycle safety**: a fully-connected
    /// graph (every node points at every other node) does not
    /// infinite-loop. The visited set guarantees each node
    /// appears at most once in the output.
    #[test]
    fn dependency_tree_handles_cycles_safely(
        nodes in arb_node_set(),
    ) {
        init_test_tracing_json();
        let node_count = nodes.len();
        let edges: Vec<CausalGraphEdge> = (0..node_count)
            .flat_map(|i| {
                (0..node_count).map(move |j| {
                    CausalGraphEdge::new(
                        format!("n{i}"),
                        format!("n{j}"),
                        CausalEvidenceKind::Observed,
                        9_000,
                        "test",
                    )
                })
            })
            .collect();
        let text = render_dependency_tree(
            &nodes,
            &edges,
            "n0",
            CausalTraversalDirection::Descendants,
            10,
            5_000,
        );
        // Each node appears at most once (via "(nN)" suffix in
        // the text). Count distinct node IDs in the output.
        for i in 0..node_count {
            let needle = format!("(n{i})");
            let count = text.matches(&needle).count();
            prop_assert!(
                count <= 1,
                "node n{i} appeared {count} times in cycle traversal"
            );
        }
    }

    /// **Render output non-empty for non-empty input**: when
    /// the focus matches at least one node, the timeline
    /// renderer emits the header + at least one entry.
    #[test]
    fn timeline_text_non_empty_for_matching_input(
        nodes in arb_node_set(),
    ) {
        init_test_tracing_json();
        let text = render_timeline_text(&nodes, &[], &FocusOptions::default());
        prop_assert!(text.starts_with("=== causal graph timeline ==="));
        // At least one entry line: not empty AND not the
        // "(no nodes match)" footer.
        prop_assert!(!text.contains("(no nodes match"));
    }

    /// **Empty FocusOptions matches all nodes**: with no
    /// filters set, the timeline contains every input node.
    #[test]
    fn empty_focus_matches_all_nodes(
        nodes in arb_node_set(),
    ) {
        init_test_tracing_json();
        let r = render_robot_toon(&nodes, &[], &FocusOptions::default());
        prop_assert_eq!(r.timeline.len(), nodes.len());
    }
}

/// Standalone non-proptest table test for marker classification.
#[test]
fn uncertainty_marker_classification_thresholds() {
    init_test_tracing_json();
    // Use the public render_robot_toon path to indirectly exercise
    // marker classification.
    let nodes = vec![
        CausalGraphNode::new("a", CausalNodeKind::PaneEvent, 10, "src"),
        CausalGraphNode::new("b", CausalNodeKind::PaneEvent, 20, "dst"),
    ];
    let edges = vec![
        CausalGraphEdge::new("a", "b", CausalEvidenceKind::Observed, 9_500, "test"),
        CausalGraphEdge::new("a", "b", CausalEvidenceKind::Observed, 6_000, "test"),
        CausalGraphEdge::new("a", "b", CausalEvidenceKind::Observed, 1_000, "test"),
    ];
    let r = render_robot_toon(&nodes, &edges, &FocusOptions::default());
    // 9_500 is Confident → not in uncertain_edges.
    // 6_000 is LowConfidence → in uncertain_edges.
    // 1_000 is Uncertain → in uncertain_edges.
    assert_eq!(r.uncertain_edges.len(), 2);
    let markers: Vec<UncertaintyMarker> = r.uncertain_edges.iter().map(|e| e.marker).collect();
    assert!(markers.contains(&UncertaintyMarker::LowConfidence));
    assert!(markers.contains(&UncertaintyMarker::Uncertain));
}
