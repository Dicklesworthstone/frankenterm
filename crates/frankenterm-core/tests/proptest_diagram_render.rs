//! Property-based tests for `diagram_render` — terminal-native tree/box/table/flow rendering.

use proptest::prelude::*;

use frankenterm_core::diagram_render::*;

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn arb_tree_leaf() -> impl Strategy<Value = TreeNode> {
    "[a-zA-Z0-9 ]{1,20}".prop_map(TreeNode::leaf)
}

fn arb_tree(max_depth: u32) -> impl Strategy<Value = TreeNode> {
    arb_tree_leaf().prop_recursive(max_depth, 50, 5, |inner| {
        ("[a-zA-Z0-9 ]{1,20}", proptest::collection::vec(inner, 1..4))
            .prop_map(|(label, children)| TreeNode::branch(label, children))
    })
}

fn arb_box_style() -> impl Strategy<Value = BoxStyle> {
    prop_oneof![
        Just(BoxStyle::Single),
        Just(BoxStyle::Double),
        Just(BoxStyle::Ascii),
    ]
}

fn arb_align() -> impl Strategy<Value = Align> {
    prop_oneof![Just(Align::Left), Just(Align::Right), Just(Align::Center),]
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // 1. render_tree output contains root label
    #[test]
    fn render_tree_contains_root_label(label in "[a-zA-Z]{1,20}") {
        let tree = TreeNode::leaf(label.clone());
        let output = render_tree(&tree);
        prop_assert!(output.contains(&label));
    }

    // 2. render_tree output contains all child labels
    #[test]
    fn render_tree_contains_all_children(
        root_label in "[a-z]{1,10}",
        child_labels in proptest::collection::vec("[a-z]{1,10}", 1..5),
    ) {
        let children: Vec<TreeNode> = child_labels.iter().map(|l| TreeNode::leaf(l)).collect();
        let tree = TreeNode::branch(root_label, children);
        let output = render_tree(&tree);
        for label in &child_labels {
            prop_assert!(output.contains(label.as_str()));
        }
    }

    // 3. TreeNode::size is always >= 1
    #[test]
    fn tree_size_at_least_one(tree in arb_tree(3)) {
        prop_assert!(tree.size() >= 1);
    }

    // 4. TreeNode::depth is always >= 1
    #[test]
    fn tree_depth_at_least_one(tree in arb_tree(3)) {
        prop_assert!(tree.depth() >= 1);
    }

    // 5. TreeNode leaf size and depth both 1
    #[test]
    fn tree_leaf_invariants(label in "[a-z]{1,10}") {
        let leaf = TreeNode::leaf(label);
        prop_assert_eq!(leaf.size(), 1);
        prop_assert_eq!(leaf.depth(), 1);
    }

    // 6. TreeNode with children has size > 1
    #[test]
    fn tree_branch_size_greater_than_one(
        label in "[a-z]{1,10}",
        child_count in 1..5usize,
    ) {
        let children: Vec<TreeNode> = (0..child_count)
            .map(|i| TreeNode::leaf(format!("c{}", i)))
            .collect();
        let tree = TreeNode::branch(label, children);
        prop_assert!(tree.size() > 1);
        prop_assert_eq!(tree.size(), 1 + child_count);
    }

    // 7. draw_box always contains the content
    #[test]
    fn draw_box_contains_content(content in "[a-z]{1,30}", style in arb_box_style()) {
        let output = draw_box(&content, style);
        prop_assert!(output.contains(&content));
    }

    // 8. draw_box output is non-empty
    #[test]
    fn draw_box_output_non_empty(content in ".{0,30}", style in arb_box_style()) {
        let output = draw_box(&content, style);
        prop_assert!(!output.is_empty());
    }

    // 9. draw_box has at least 3 lines (top border, content, bottom border)
    #[test]
    fn draw_box_minimum_lines(content in "[a-z]{1,20}", style in arb_box_style()) {
        let output = draw_box(&content, style);
        let line_count = output.lines().count();
        prop_assert!(line_count >= 3, "Expected >= 3 lines, got {}", line_count);
    }

    // 10. draw_box multiline: line count = content lines + 2 borders
    #[test]
    fn draw_box_line_count_matches(
        lines in proptest::collection::vec("[a-z]{1,10}", 1..5),
    ) {
        let content = lines.join("\n");
        let output = draw_box(&content, BoxStyle::Single);
        let output_lines = output.lines().count();
        // content lines + top border + bottom border
        prop_assert_eq!(output_lines, lines.len() + 2);
    }

    // 11. DiagramTable len tracks rows added
    #[test]
    fn table_len_tracks_rows(row_count in 0..20usize) {
        let mut table = DiagramTable::new(vec!["A".into()]);
        for i in 0..row_count {
            table.add_row(vec![format!("{}", i)]);
        }
        prop_assert_eq!(table.len(), row_count);
    }

    // 12. DiagramTable is_empty consistent with len
    #[test]
    fn table_is_empty_consistent(row_count in 0..10usize) {
        let mut table = DiagramTable::new(vec!["A".into()]);
        for i in 0..row_count {
            table.add_row(vec![format!("{}", i)]);
        }
        prop_assert_eq!(table.is_empty(), row_count == 0);
    }

    // 13. DiagramTable render contains headers
    #[test]
    fn table_render_contains_headers(
        headers in proptest::collection::vec("[a-z]{1,10}", 1..4),
    ) {
        let table = DiagramTable::new(headers.clone());
        let output = table.render();
        for h in &headers {
            prop_assert!(output.contains(h.as_str()));
        }
    }

    // 14. DiagramTable render contains cell data
    #[test]
    fn table_render_contains_cells(value in "[a-z]{1,15}") {
        let mut table = DiagramTable::new(vec!["Col".into()]);
        table.add_row(vec![value.clone()]);
        let output = table.render();
        prop_assert!(output.contains(&value));
    }

    // 15. DiagramTable alignment doesn't lose data
    #[test]
    fn table_alignment_preserves_data(align in arb_align(), value in "[a-z]{1,10}") {
        let mut table = DiagramTable::new(vec!["H".into()])
            .with_alignments(vec![align]);
        table.add_row(vec![value.clone()]);
        let output = table.render();
        prop_assert!(output.contains(&value));
    }

    // 16. render_flow empty produces empty string
    #[test]
    fn render_flow_empty_is_empty(_dummy in 0..1u8) {
        let output = render_flow(&[], &[]);
        prop_assert!(output.is_empty());
    }

    // 17. render_flow single node has no arrow
    #[test]
    fn render_flow_single_no_arrow(label in "[a-z]{1,15}") {
        let nodes = vec![FlowNode { id: "a".into(), label }];
        let output = render_flow(&nodes, &[]);
        prop_assert!(!output.contains("▼"));
    }

    // 18. render_flow with edge has arrow
    #[test]
    fn render_flow_edge_has_arrow(
        label_a in "[a-z]{1,10}",
        label_b in "[a-z]{1,10}",
    ) {
        let nodes = vec![
            FlowNode { id: "a".into(), label: label_a },
            FlowNode { id: "b".into(), label: label_b },
        ];
        let edges = vec![FlowEdge { from: "a".into(), to: "b".into(), label: None }];
        let output = render_flow(&nodes, &edges);
        prop_assert!(output.contains("▼"));
    }

    // 19. render_topology empty shows "empty"
    #[test]
    fn render_topology_empty_message(_dummy in 0..1u8) {
        let output = render_topology(&[]);
        prop_assert!(output.contains("empty"));
    }

    // 20. render_topology single pane contains pane id
    #[test]
    fn render_topology_single_contains_id(id in 1..1000u64) {
        let entries = vec![(id, None, "shell".into())];
        let output = render_topology(&entries);
        let id_str = format!("[{}]", id);
        prop_assert!(output.contains(&id_str));
    }

    // 21. render_topology parent-child: both IDs appear
    #[test]
    fn render_topology_parent_child_ids(
        parent_id in 1..500u64,
        child_id in 501..1000u64,
    ) {
        let entries = vec![
            (parent_id, None, "parent".into()),
            (child_id, Some(parent_id), "child".into()),
        ];
        let output = render_topology(&entries);
        let parent_tag = format!("[{}]", parent_id);
        let child_tag = format!("[{}]", child_id);
        prop_assert!(output.contains(&parent_tag));
        prop_assert!(output.contains(&child_tag));
    }

    // 22. render_topology multiple roots shows "topology" wrapper
    #[test]
    fn render_topology_multi_root_wrapper(
        id1 in 1..500u64,
        id2 in 501..1000u64,
    ) {
        let entries = vec![
            (id1, None, "left".into()),
            (id2, None, "right".into()),
        ];
        let output = render_topology(&entries);
        prop_assert!(output.contains("topology"));
    }

    // 23. render_dependency_graph empty shows "no dependencies"
    #[test]
    fn render_dep_graph_empty(_dummy in 0..1u8) {
        let output = render_dependency_graph(&[]);
        prop_assert!(output.contains("no dependencies"));
    }

    // 24. render_dependency_graph contains all labels
    #[test]
    fn render_dep_graph_contains_labels(
        labels in proptest::collection::vec("[a-z]{2,10}", 1..4),
    ) {
        let items: Vec<(String, String, Vec<String>)> = labels.iter()
            .enumerate()
            .map(|(i, l)| (format!("n{}", i), l.clone(), vec![]))
            .collect();
        let output = render_dependency_graph(&items);
        for label in &labels {
            prop_assert!(output.contains(label.as_str()));
        }
    }

    // 25. BoxStyle equality is reflexive
    #[test]
    fn box_style_eq_reflexive(style in arb_box_style()) {
        prop_assert_eq!(style, style);
    }

    // 26. Align equality is reflexive
    #[test]
    fn align_eq_reflexive(align in arb_align()) {
        prop_assert_eq!(align, align);
    }

    // 27. render_tree never panics for arbitrary trees
    #[test]
    fn render_tree_never_panics(tree in arb_tree(4)) {
        let output = render_tree(&tree);
        prop_assert!(!output.is_empty());
    }

    // 28. DiagramTable render has border characters
    #[test]
    fn table_render_has_borders(value in "[a-z]{1,10}") {
        let mut table = DiagramTable::new(vec!["H".into()]);
        table.add_row(vec![value]);
        let output = table.render();
        prop_assert!(output.contains("┌"));
        prop_assert!(output.contains("┘"));
        prop_assert!(output.contains("│"));
        prop_assert!(output.contains("─"));
    }

    // 29. render_tree line count >= size (each node gets at least one line)
    #[test]
    fn render_tree_lines_ge_size(tree in arb_tree(3)) {
        let output = render_tree(&tree);
        let line_count = output.lines().count();
        prop_assert!(
            line_count >= tree.size(),
            "lines {} < size {}",
            line_count, tree.size()
        );
    }

    // 30. FlowNode label preserved in render_flow
    #[test]
    fn flow_node_label_preserved(label in "[a-z]{2,15}") {
        let nodes = vec![FlowNode { id: "x".into(), label: label.clone() }];
        let output = render_flow(&nodes, &[]);
        prop_assert!(output.contains(&label));
    }
}
