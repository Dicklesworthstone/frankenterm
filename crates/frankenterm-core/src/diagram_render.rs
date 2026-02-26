//! Terminal-native diagram rendering with box-drawing characters.
//!
//! Provides lightweight, dependency-free rendering of:
//! - Tree views for hierarchical data (pane topology, bead dependencies)
//! - Box drawings for flowcharts
//! - ASCII tables for structured data
//!
//! Feature-gated behind `diagram-viz`.

use std::fmt::Write;

// =============================================================================
// Tree rendering
// =============================================================================

/// A node in a tree structure for rendering.
#[derive(Debug, Clone)]
pub struct TreeNode {
    /// Display label for this node.
    pub label: String,
    /// Child nodes.
    pub children: Vec<TreeNode>,
}

impl TreeNode {
    /// Create a leaf node (no children).
    pub fn leaf(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            children: vec![],
        }
    }

    /// Create a node with children.
    pub fn branch(label: impl Into<String>, children: Vec<TreeNode>) -> Self {
        Self {
            label: label.into(),
            children,
        }
    }

    /// Total number of nodes in this subtree (including self).
    pub fn size(&self) -> usize {
        1 + self.children.iter().map(|c| c.size()).sum::<usize>()
    }

    /// Depth of this subtree (1 for a leaf).
    pub fn depth(&self) -> usize {
        1 + self.children.iter().map(|c| c.depth()).max().unwrap_or(0)
    }
}

/// Render a tree using Unicode box-drawing characters.
///
/// Output example:
/// ```text
/// root
/// ├── child1
/// │   └── grandchild
/// └── child2
/// ```
pub fn render_tree(root: &TreeNode) -> String {
    let mut buf = String::new();
    writeln!(buf, "{}", root.label).ok();
    render_tree_children(&mut buf, &root.children, "");
    buf
}

fn render_tree_children(buf: &mut String, children: &[TreeNode], prefix: &str) {
    for (i, child) in children.iter().enumerate() {
        let is_last = i == children.len() - 1;
        let connector = if is_last { "└── " } else { "├── " };
        let child_prefix = if is_last { "    " } else { "│   " };

        writeln!(buf, "{}{}{}", prefix, connector, child.label).ok();
        render_tree_children(buf, &child.children, &format!("{}{}", prefix, child_prefix));
    }
}

// =============================================================================
// Box drawing
// =============================================================================

/// Style for box borders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoxStyle {
    /// ┌─┐│└─┘
    Single,
    /// ╔═╗║╚═╝
    Double,
    /// +--+|+--+
    Ascii,
}

impl BoxStyle {
    fn chars(self) -> BoxChars {
        match self {
            Self::Single => BoxChars {
                tl: '┌',
                tr: '┐',
                bl: '└',
                br: '┘',
                h: '─',
                v: '│',
            },
            Self::Double => BoxChars {
                tl: '╔',
                tr: '╗',
                bl: '╚',
                br: '╝',
                h: '═',
                v: '║',
            },
            Self::Ascii => BoxChars {
                tl: '+',
                tr: '+',
                bl: '+',
                br: '+',
                h: '-',
                v: '|',
            },
        }
    }
}

struct BoxChars {
    tl: char,
    tr: char,
    bl: char,
    br: char,
    h: char,
    v: char,
}

/// Draw a box around text content.
///
/// Each line of content is padded to the width of the longest line.
pub fn draw_box(content: &str, style: BoxStyle) -> String {
    let c = style.chars();
    let lines: Vec<&str> = content.lines().collect();
    let max_width = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0);

    let mut buf = String::new();
    // Top border
    write!(buf, "{}", c.tl).ok();
    for _ in 0..max_width + 2 {
        write!(buf, "{}", c.h).ok();
    }
    writeln!(buf, "{}", c.tr).ok();

    // Content lines
    for line in &lines {
        let padding = max_width - line.chars().count();
        write!(buf, "{} {}", c.v, line).ok();
        for _ in 0..padding {
            write!(buf, " ").ok();
        }
        writeln!(buf, " {}", c.v).ok();
    }

    // Bottom border
    write!(buf, "{}", c.bl).ok();
    for _ in 0..max_width + 2 {
        write!(buf, "{}", c.h).ok();
    }
    writeln!(buf, "{}", c.br).ok();

    buf
}

/// A labeled box in a flowchart.
#[derive(Debug, Clone)]
pub struct FlowNode {
    /// Node identifier.
    pub id: String,
    /// Display label.
    pub label: String,
}

/// An edge in a flowchart.
#[derive(Debug, Clone)]
pub struct FlowEdge {
    pub from: String,
    pub to: String,
    /// Optional edge label.
    pub label: Option<String>,
}

/// Render a simple vertical flowchart.
///
/// Nodes are rendered top-to-bottom with arrows between them.
pub fn render_flow(nodes: &[FlowNode], edges: &[FlowEdge]) -> String {
    if nodes.is_empty() {
        return String::new();
    }

    // Build adjacency for ordering
    let ordered = topological_order(nodes, edges);
    let mut buf = String::new();

    for (i, node) in ordered.iter().enumerate() {
        let boxed = draw_box(&node.label, BoxStyle::Single);
        for line in boxed.lines() {
            writeln!(buf, "  {}", line).ok();
        }

        // Draw arrow if not last
        if i < ordered.len() - 1 {
            // Find edge label
            let edge_label = edges
                .iter()
                .find(|e| {
                    e.from == node.id && ordered.get(i + 1).is_some_and(|next| next.id == e.to)
                })
                .and_then(|e| e.label.as_deref());

            if let Some(lbl) = edge_label {
                writeln!(buf, "    │ {}", lbl).ok();
            } else {
                writeln!(buf, "    │").ok();
            }
            writeln!(buf, "    ▼").ok();
        }
    }

    buf
}

/// Simple topological ordering: follow edges, fallback to input order.
fn topological_order<'a>(nodes: &'a [FlowNode], edges: &[FlowEdge]) -> Vec<&'a FlowNode> {
    use std::collections::{HashMap, HashSet, VecDeque};

    let id_to_idx: HashMap<&str, usize> = nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.id.as_str(), i))
        .collect();

    let mut in_degree = vec![0usize; nodes.len()];
    let mut adj: Vec<Vec<usize>> = vec![vec![]; nodes.len()];

    for edge in edges {
        if let (Some(&from), Some(&to)) = (
            id_to_idx.get(edge.from.as_str()),
            id_to_idx.get(edge.to.as_str()),
        ) {
            adj[from].push(to);
            in_degree[to] += 1;
        }
    }

    let mut queue: VecDeque<usize> = in_degree
        .iter()
        .enumerate()
        .filter(|(_, d)| **d == 0)
        .map(|(i, _)| i)
        .collect();

    let mut result = Vec::with_capacity(nodes.len());
    let mut visited = HashSet::new();

    while let Some(idx) = queue.pop_front() {
        if visited.insert(idx) {
            result.push(&nodes[idx]);
            for &next in &adj[idx] {
                in_degree[next] -= 1;
                if in_degree[next] == 0 {
                    queue.push_back(next);
                }
            }
        }
    }

    // Append any nodes not reached (disconnected components)
    for (i, node) in nodes.iter().enumerate() {
        if !visited.contains(&i) {
            result.push(node);
        }
    }

    result
}

// =============================================================================
// Table rendering
// =============================================================================

/// Column alignment for table rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
    Center,
}

/// A simple ASCII table renderer.
#[derive(Debug, Clone)]
pub struct DiagramTable {
    headers: Vec<String>,
    alignments: Vec<Align>,
    rows: Vec<Vec<String>>,
}

impl DiagramTable {
    /// Create a table with headers.
    pub fn new(headers: Vec<String>) -> Self {
        let n = headers.len();
        Self {
            headers,
            alignments: vec![Align::Left; n],
            rows: vec![],
        }
    }

    /// Set column alignments.
    #[must_use]
    pub fn with_alignments(mut self, alignments: Vec<Align>) -> Self {
        self.alignments = alignments;
        self
    }

    /// Add a row.
    pub fn add_row(&mut self, row: Vec<String>) {
        self.rows.push(row);
    }

    /// Number of rows (excluding header).
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Whether the table has no data rows.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Render the table as a string with box-drawing borders.
    pub fn render(&self) -> String {
        let col_count = self.headers.len();
        let mut widths = vec![0usize; col_count];

        // Compute column widths
        for (i, h) in self.headers.iter().enumerate() {
            widths[i] = widths[i].max(h.chars().count());
        }
        for row in &self.rows {
            for (i, cell) in row.iter().enumerate() {
                if i < col_count {
                    widths[i] = widths[i].max(cell.chars().count());
                }
            }
        }

        let mut buf = String::new();

        // Top border
        self.write_separator(&mut buf, &widths, '┌', '┬', '┐');
        // Header
        self.write_row(&mut buf, &self.headers, &widths);
        // Header separator
        self.write_separator(&mut buf, &widths, '├', '┼', '┤');
        // Data rows
        for row in &self.rows {
            self.write_row(&mut buf, row, &widths);
        }
        // Bottom border
        self.write_separator(&mut buf, &widths, '└', '┴', '┘');

        buf
    }

    #[allow(clippy::unused_self)]
    fn write_separator(
        &self,
        buf: &mut String,
        widths: &[usize],
        left: char,
        mid: char,
        right: char,
    ) {
        write!(buf, "{}", left).ok();
        for (i, &w) in widths.iter().enumerate() {
            for _ in 0..w + 2 {
                write!(buf, "─").ok();
            }
            if i < widths.len() - 1 {
                write!(buf, "{}", mid).ok();
            }
        }
        writeln!(buf, "{}", right).ok();
    }

    fn write_row(&self, buf: &mut String, cells: &[String], widths: &[usize]) {
        write!(buf, "│").ok();
        for (i, &w) in widths.iter().enumerate() {
            let cell = cells.get(i).map(|s| s.as_str()).unwrap_or("");
            let cell_len = cell.chars().count();
            let padding = w.saturating_sub(cell_len);
            let align = self.alignments.get(i).copied().unwrap_or(Align::Left);

            match align {
                Align::Left => {
                    write!(buf, " {}", cell).ok();
                    for _ in 0..padding {
                        write!(buf, " ").ok();
                    }
                    write!(buf, " ").ok();
                }
                Align::Right => {
                    write!(buf, " ").ok();
                    for _ in 0..padding {
                        write!(buf, " ").ok();
                    }
                    write!(buf, "{} ", cell).ok();
                }
                Align::Center => {
                    let left_pad = padding / 2;
                    let right_pad = padding - left_pad;
                    write!(buf, " ").ok();
                    for _ in 0..left_pad {
                        write!(buf, " ").ok();
                    }
                    write!(buf, "{}", cell).ok();
                    for _ in 0..right_pad {
                        write!(buf, " ").ok();
                    }
                    write!(buf, " ").ok();
                }
            }
            write!(buf, "│").ok();
        }
        writeln!(buf).ok();
    }
}

// =============================================================================
// Convenience helpers
// =============================================================================

/// Render a pane topology as a tree.
///
/// Expects (pane_id, parent_id, label) triples.
pub fn render_topology(entries: &[(u64, Option<u64>, String)]) -> String {
    use std::collections::HashMap;

    if entries.is_empty() {
        return String::from("(empty topology)\n");
    }

    // Build parent→children map
    let mut children_map: HashMap<u64, Vec<(u64, &str)>> = HashMap::new();
    let mut roots = Vec::new();

    for (id, parent, label) in entries {
        if let Some(pid) = parent {
            children_map.entry(*pid).or_default().push((*id, label));
        } else {
            roots.push((*id, label.as_str()));
        }
    }

    fn build_tree(id: u64, label: &str, children_map: &HashMap<u64, Vec<(u64, &str)>>) -> TreeNode {
        let children = children_map
            .get(&id)
            .map(|kids| {
                kids.iter()
                    .map(|(kid_id, kid_label)| build_tree(*kid_id, kid_label, children_map))
                    .collect()
            })
            .unwrap_or_default();
        TreeNode::branch(format!("[{}] {}", id, label), children)
    }

    if roots.len() == 1 {
        let (id, label) = roots[0];
        render_tree(&build_tree(id, label, &children_map))
    } else {
        // Multiple roots: wrap in a virtual root
        let virtual_children: Vec<TreeNode> = roots
            .iter()
            .map(|(id, label)| build_tree(*id, label, &children_map))
            .collect();
        let root = TreeNode::branch("topology", virtual_children);
        render_tree(&root)
    }
}

/// Render a dependency graph as a vertical flowchart.
pub fn render_dependency_graph(items: &[(String, String, Vec<String>)]) -> String {
    let nodes: Vec<FlowNode> = items
        .iter()
        .map(|(id, label, _)| FlowNode {
            id: id.clone(),
            label: label.clone(),
        })
        .collect();

    let edges: Vec<FlowEdge> = items
        .iter()
        .flat_map(|(id, _, deps)| {
            deps.iter().map(move |dep| FlowEdge {
                from: dep.clone(),
                to: id.clone(),
                label: None,
            })
        })
        .collect();

    if nodes.is_empty() {
        return String::from("(no dependencies)\n");
    }

    render_flow(&nodes, &edges)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -- TreeNode --

    #[test]
    fn tree_leaf_size_is_one() {
        let leaf = TreeNode::leaf("x");
        assert_eq!(leaf.size(), 1);
        assert_eq!(leaf.depth(), 1);
    }

    #[test]
    fn tree_branch_size() {
        let tree = TreeNode::branch(
            "root",
            vec![
                TreeNode::leaf("a"),
                TreeNode::branch("b", vec![TreeNode::leaf("c")]),
            ],
        );
        assert_eq!(tree.size(), 4);
        assert_eq!(tree.depth(), 3);
    }

    #[test]
    fn render_tree_single_leaf() {
        let tree = TreeNode::leaf("hello");
        let output = render_tree(&tree);
        assert!(output.contains("hello"));
        assert!(!output.contains("├"));
    }

    #[test]
    fn render_tree_with_children() {
        let tree = TreeNode::branch(
            "root",
            vec![TreeNode::leaf("child1"), TreeNode::leaf("child2")],
        );
        let output = render_tree(&tree);
        assert!(output.contains("root"));
        assert!(output.contains("├── child1"));
        assert!(output.contains("└── child2"));
    }

    #[test]
    fn render_tree_nested() {
        let tree = TreeNode::branch("A", vec![TreeNode::branch("B", vec![TreeNode::leaf("C")])]);
        let output = render_tree(&tree);
        assert!(output.contains("A"));
        assert!(output.contains("└── B"));
        assert!(output.contains("    └── C"));
    }

    // -- Box drawing --

    #[test]
    fn draw_box_single_line() {
        let output = draw_box("hello", BoxStyle::Single);
        assert!(output.contains("┌"));
        assert!(output.contains("┐"));
        assert!(output.contains("└"));
        assert!(output.contains("┘"));
        assert!(output.contains("hello"));
    }

    #[test]
    fn draw_box_double() {
        let output = draw_box("test", BoxStyle::Double);
        assert!(output.contains("╔"));
        assert!(output.contains("╝"));
    }

    #[test]
    fn draw_box_ascii() {
        let output = draw_box("test", BoxStyle::Ascii);
        assert!(output.contains("+"));
        assert!(output.contains("|"));
        assert!(output.contains("-"));
    }

    #[test]
    fn draw_box_multiline() {
        let output = draw_box("line1\nline2\nline3", BoxStyle::Single);
        assert!(output.contains("line1"));
        assert!(output.contains("line2"));
        assert!(output.contains("line3"));
        // Lines should be padded to same width
        assert!(output.lines().count() >= 5); // top + 3 content + bottom
    }

    #[test]
    fn draw_box_empty_content() {
        let output = draw_box("", BoxStyle::Single);
        assert!(output.contains("┌"));
        assert!(output.contains("└"));
    }

    // -- Flow rendering --

    #[test]
    fn render_flow_single_node() {
        let nodes = vec![FlowNode {
            id: "a".into(),
            label: "Start".into(),
        }];
        let output = render_flow(&nodes, &[]);
        assert!(output.contains("Start"));
        assert!(!output.contains("▼"));
    }

    #[test]
    fn render_flow_two_nodes() {
        let nodes = vec![
            FlowNode {
                id: "a".into(),
                label: "Start".into(),
            },
            FlowNode {
                id: "b".into(),
                label: "End".into(),
            },
        ];
        let edges = vec![FlowEdge {
            from: "a".into(),
            to: "b".into(),
            label: None,
        }];
        let output = render_flow(&nodes, &edges);
        assert!(output.contains("Start"));
        assert!(output.contains("End"));
        assert!(output.contains("▼"));
    }

    #[test]
    fn render_flow_with_edge_label() {
        let nodes = vec![
            FlowNode {
                id: "a".into(),
                label: "A".into(),
            },
            FlowNode {
                id: "b".into(),
                label: "B".into(),
            },
        ];
        let edges = vec![FlowEdge {
            from: "a".into(),
            to: "b".into(),
            label: Some("next".into()),
        }];
        let output = render_flow(&nodes, &edges);
        assert!(output.contains("next"));
    }

    #[test]
    fn render_flow_empty() {
        let output = render_flow(&[], &[]);
        assert!(output.is_empty());
    }

    // -- Topological ordering --

    #[test]
    fn topological_order_linear() {
        let nodes = vec![
            FlowNode {
                id: "c".into(),
                label: "C".into(),
            },
            FlowNode {
                id: "b".into(),
                label: "B".into(),
            },
            FlowNode {
                id: "a".into(),
                label: "A".into(),
            },
        ];
        let edges = vec![
            FlowEdge {
                from: "a".into(),
                to: "b".into(),
                label: None,
            },
            FlowEdge {
                from: "b".into(),
                to: "c".into(),
                label: None,
            },
        ];
        let ordered = topological_order(&nodes, &edges);
        let ids: Vec<&str> = ordered.iter().map(|n| n.id.as_str()).collect();
        let a_pos = ids.iter().position(|&id| id == "a").unwrap();
        let b_pos = ids.iter().position(|&id| id == "b").unwrap();
        let c_pos = ids.iter().position(|&id| id == "c").unwrap();
        assert!(a_pos < b_pos);
        assert!(b_pos < c_pos);
    }

    #[test]
    fn topological_order_disconnected() {
        let nodes = vec![
            FlowNode {
                id: "x".into(),
                label: "X".into(),
            },
            FlowNode {
                id: "y".into(),
                label: "Y".into(),
            },
        ];
        let ordered = topological_order(&nodes, &[]);
        assert_eq!(ordered.len(), 2);
    }

    // -- DiagramTable --

    #[test]
    fn table_empty() {
        let table = DiagramTable::new(vec!["Col1".into(), "Col2".into()]);
        assert!(table.is_empty());
        assert_eq!(table.len(), 0);
        let output = table.render();
        assert!(output.contains("Col1"));
        assert!(output.contains("Col2"));
    }

    #[test]
    fn table_with_rows() {
        let mut table = DiagramTable::new(vec!["Name".into(), "Value".into()]);
        table.add_row(vec!["foo".into(), "42".into()]);
        table.add_row(vec!["bar".into(), "99".into()]);
        assert_eq!(table.len(), 2);
        let output = table.render();
        assert!(output.contains("foo"));
        assert!(output.contains("42"));
        assert!(output.contains("bar"));
        assert!(output.contains("99"));
    }

    #[test]
    fn table_right_aligned() {
        let mut table = DiagramTable::new(vec!["N".into(), "V".into()])
            .with_alignments(vec![Align::Left, Align::Right]);
        table.add_row(vec!["x".into(), "123".into()]);
        let output = table.render();
        assert!(output.contains("123"));
    }

    #[test]
    fn table_center_aligned() {
        let mut table =
            DiagramTable::new(vec!["Header".into()]).with_alignments(vec![Align::Center]);
        table.add_row(vec!["Hi".into()]);
        let output = table.render();
        assert!(output.contains("Hi"));
    }

    #[test]
    fn table_borders_present() {
        let mut table = DiagramTable::new(vec!["A".into()]);
        table.add_row(vec!["1".into()]);
        let output = table.render();
        assert!(output.contains("┌"));
        assert!(output.contains("┐"));
        assert!(output.contains("├"));
        assert!(output.contains("┤"));
        assert!(output.contains("└"));
        assert!(output.contains("┘"));
        assert!(output.contains("─"));
        assert!(output.contains("│"));
    }

    // -- Topology rendering --

    #[test]
    fn render_topology_empty() {
        let output = render_topology(&[]);
        assert!(output.contains("empty"));
    }

    #[test]
    fn render_topology_single_pane() {
        let entries = vec![(1, None, "shell".into())];
        let output = render_topology(&entries);
        assert!(output.contains("[1] shell"));
    }

    #[test]
    fn render_topology_parent_child() {
        let entries = vec![(1, None, "root".into()), (2, Some(1), "child".into())];
        let output = render_topology(&entries);
        assert!(output.contains("[1] root"));
        assert!(output.contains("[2] child"));
    }

    #[test]
    fn render_topology_multiple_roots() {
        let entries = vec![(1, None, "left".into()), (2, None, "right".into())];
        let output = render_topology(&entries);
        assert!(output.contains("topology"));
        assert!(output.contains("[1] left"));
        assert!(output.contains("[2] right"));
    }

    // -- Dependency graph --

    #[test]
    fn render_dependency_graph_empty() {
        let output = render_dependency_graph(&[]);
        assert!(output.contains("no dependencies"));
    }

    #[test]
    fn render_dependency_graph_chain() {
        let items = vec![
            ("a".into(), "Step A".into(), vec![]),
            ("b".into(), "Step B".into(), vec!["a".into()]),
            ("c".into(), "Step C".into(), vec!["b".into()]),
        ];
        let output = render_dependency_graph(&items);
        assert!(output.contains("Step A"));
        assert!(output.contains("Step B"));
        assert!(output.contains("Step C"));
    }

    // -- BoxStyle --

    #[test]
    fn box_style_all_variants_produce_output() {
        for style in [BoxStyle::Single, BoxStyle::Double, BoxStyle::Ascii] {
            let output = draw_box("test", style);
            assert!(!output.is_empty());
            assert!(output.contains("test"));
        }
    }

    #[test]
    fn box_style_eq() {
        assert_eq!(BoxStyle::Single, BoxStyle::Single);
        assert_ne!(BoxStyle::Single, BoxStyle::Double);
    }

    // -- Edge cases --

    #[test]
    fn tree_deep_nesting() {
        let mut node = TreeNode::leaf("leaf");
        for i in 0..10 {
            node = TreeNode::branch(format!("level-{}", i), vec![node]);
        }
        let output = render_tree(&node);
        assert!(output.contains("leaf"));
        assert!(output.contains("level-0"));
        assert_eq!(node.depth(), 11);
    }

    #[test]
    fn table_missing_cells() {
        let mut table = DiagramTable::new(vec!["A".into(), "B".into(), "C".into()]);
        table.add_row(vec!["1".into()]); // Missing B, C
        let output = table.render();
        assert!(output.contains("1"));
        // Should not panic with fewer cells than columns
    }
}
