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
// Bounded Mermaid parser & terminal renderer (ft-xxfwy.54)
// =============================================================================

/// Maximum Mermaid input size accepted by [`render_mermaid`] (64 KiB).
pub const MERMAID_MAX_INPUT_BYTES: usize = 64 * 1024;
/// Maximum nodes admitted in a single Mermaid diagram.
pub const MERMAID_MAX_NODES: usize = 256;
/// Maximum edges / messages admitted in a single Mermaid diagram.
pub const MERMAID_MAX_EDGES: usize = 512;
/// Maximum rendered output characters emitted by [`render_mermaid`] (256 KiB).
pub const MERMAID_MAX_OUTPUT_CHARS: usize = 256 * 1024;

/// Supported Mermaid diagram families for terminal rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MermaidDiagramKind {
    Flowchart,
    Sequence,
}

/// Resource bounds and visual options for [`render_mermaid`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MermaidRenderOptions {
    pub style: BoxStyle,
    pub max_input_bytes: usize,
    pub max_nodes: usize,
    pub max_edges: usize,
    pub max_output_chars: usize,
}

impl Default for MermaidRenderOptions {
    fn default() -> Self {
        Self {
            style: BoxStyle::Single,
            max_input_bytes: MERMAID_MAX_INPUT_BYTES,
            max_nodes: MERMAID_MAX_NODES,
            max_edges: MERMAID_MAX_EDGES,
            max_output_chars: MERMAID_MAX_OUTPUT_CHARS,
        }
    }
}

/// Typed, content-free error returned by [`render_mermaid`].
#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "error", rename_all = "snake_case")]
pub enum MermaidRenderError {
    #[error("mermaid input is empty")]
    EmptyInput,
    #[error("mermaid input size {bytes} exceeds limit {max_bytes}")]
    InputTooLarge { bytes: usize, max_bytes: usize },
    #[error(
        "unsupported mermaid diagram header '{header}' (supported: flowchart, graph, sequenceDiagram)"
    )]
    UnsupportedDiagramType { header: String },
    #[error("mermaid node count {nodes} exceeds limit {max_nodes}")]
    NodeLimitExceeded { nodes: usize, max_nodes: usize },
    #[error("mermaid edge count {edges} exceeds limit {max_edges}")]
    EdgeLimitExceeded { edges: usize, max_edges: usize },
    #[error("mermaid syntax error at line {line}: {reason}")]
    SyntaxError { line: usize, reason: String },
    #[error("mermaid rendered output length {chars} exceeds limit {max_chars}")]
    OutputTooLarge { chars: usize, max_chars: usize },
}

/// Structured output from [`render_mermaid`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MermaidRenderResult {
    pub kind: MermaidDiagramKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub direction: Option<String>,
    pub node_count: usize,
    pub edge_count: usize,
    pub rendered: String,
}

fn parse_mermaid_node_token(raw: &str) -> Result<(String, Option<String>), &'static str> {
    let trimmed = raw.trim().trim_end_matches(';').trim();
    if trimmed.is_empty() {
        return Err("empty node identifier");
    }
    let delims: &[(&str, &str)] = &[
        ("((", "))"),
        ("([", "])"),
        ("[[", "]]"),
        ("[(", ")]"),
        ("{{", "}}"),
        ("[", "]"),
        ("(", ")"),
        ("{", "}"),
    ];
    for &(open, close) in delims {
        if let Some(open_idx) = trimmed.find(open) {
            let id = trimmed[..open_idx].trim();
            let rest = &trimmed[open_idx + open.len()..];
            if let Some(label_body) = rest.strip_suffix(close) {
                if id.is_empty() {
                    return Err("node shape missing identifier");
                }
                let label = label_body.trim().trim_matches('"').trim_matches('\'');
                return Ok((id.to_string(), Some(label.to_string())));
            }
        }
    }
    if trimmed.chars().any(char::is_whitespace) {
        return Err("node identifier contains whitespace outside label brackets");
    }
    Ok((trimmed.to_string(), None))
}

fn split_mermaid_edge_once(line: &str) -> Option<(&str, Option<String>, &str)> {
    const OPERATORS: &[&str] = &["-.->", "==>", "-->", "---", "->"];
    let mut best: Option<(usize, &str)> = None;
    for &op in OPERATORS {
        if let Some(idx) = line.find(op)
            && best.is_none_or(|(best_idx, best_op)| {
                idx < best_idx || (idx == best_idx && op.len() > best_op.len())
            })
        {
            best = Some((idx, op));
        }
    }
    let (idx, op) = best?;
    let lhs = line[..idx].trim();
    let mut rhs = line[idx + op.len()..].trim();
    let mut label = None;
    if let Some(after_pipe) = rhs.strip_prefix('|')
        && let Some(end_pipe) = after_pipe.find('|')
    {
        let text = after_pipe[..end_pipe].trim();
        if !text.is_empty() {
            label = Some(text.to_string());
        }
        rhs = after_pipe[end_pipe + 1..].trim();
    }
    Some((lhs, label, rhs))
}

fn render_flowchart_styled(
    nodes: &[FlowNode],
    edges: &[FlowEdge],
    direction: &str,
    style: BoxStyle,
) -> String {
    if nodes.is_empty() {
        return String::new();
    }
    let ordered = topological_order(nodes, edges);
    let mut buf = String::new();

    if matches!(direction, "LR" | "RL") {
        let mut items: Vec<&FlowNode> = ordered;
        if direction == "RL" {
            items.reverse();
        }
        let mut parts = Vec::with_capacity(items.len());
        for (i, node) in items.iter().enumerate() {
            parts.push(format!("[{}]", node.label));
            if i + 1 < items.len() {
                let next = items[i + 1];
                let edge_label = edges
                    .iter()
                    .find(|e| {
                        (e.from == node.id && e.to == next.id)
                            || (direction == "RL" && e.from == next.id && e.to == node.id)
                    })
                    .and_then(|e| e.label.as_deref());
                let arrow = if direction == "RL" {
                    "◀──"
                } else {
                    "──▶"
                };
                if let Some(lbl) = edge_label {
                    parts.push(format!("─({lbl})─{arrow}"));
                } else {
                    parts.push(arrow.to_string());
                }
            }
        }
        let joined = parts.join(" ");
        return draw_box(&joined, style);
    }

    for (i, node) in ordered.iter().enumerate() {
        let boxed = draw_box(&node.label, style);
        for line in boxed.lines() {
            writeln!(buf, "  {line}").ok();
        }
        if i + 1 < ordered.len() {
            let next = ordered[i + 1];
            let edge_label = edges
                .iter()
                .find(|e| e.from == node.id && e.to == next.id)
                .and_then(|e| e.label.as_deref());
            if let Some(lbl) = edge_label {
                writeln!(buf, "    │ {lbl}").ok();
            } else {
                writeln!(buf, "    │").ok();
            }
            writeln!(buf, "    ▼").ok();
        }
    }
    buf
}

fn render_sequence_diagram(
    participants: &[(String, String)],
    messages: &[(String, String, String, bool)],
    style: BoxStyle,
) -> String {
    let mut table = DiagramTable::new(vec![
        "From".to_string(),
        "Flow".to_string(),
        "To".to_string(),
        "Message".to_string(),
    ]);
    let label_for = |id: &str| -> String {
        participants
            .iter()
            .find(|(pid, _)| pid == id)
            .map_or_else(|| id.to_string(), |(_, label)| label.clone())
    };
    for (from, to, msg, dashed) in messages {
        let arrow = if *dashed { "──▷" } else { "──▶" };
        table.add_row(vec![
            label_for(from),
            arrow.to_string(),
            label_for(to),
            msg.clone(),
        ]);
    }
    let header_line = participants
        .iter()
        .map(|(_, label)| format!("[{label}]"))
        .collect::<Vec<_>>()
        .join("   ");
    let mut out = draw_box(&header_line, style);
    if !messages.is_empty() {
        out.push_str(&table.render());
    }
    out
}

/// Parse and render a bounded Mermaid diagram (`flowchart`, `graph`, or
/// `sequenceDiagram`) into terminal-native box-drawing output.
pub fn render_mermaid(
    source: &str,
    options: &MermaidRenderOptions,
) -> Result<MermaidRenderResult, MermaidRenderError> {
    if source.len() > options.max_input_bytes {
        return Err(MermaidRenderError::InputTooLarge {
            bytes: source.len(),
            max_bytes: options.max_input_bytes,
        });
    }
    let stripped = source.strip_prefix('\u{feff}').unwrap_or(source);
    let mut lines: Vec<(usize, &str)> = Vec::new();
    for (idx, raw) in stripped.lines().enumerate() {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with("%%") || trimmed.starts_with("```") {
            continue;
        }
        lines.push((idx + 1, trimmed));
    }
    let Some(&(header_line, header)) = lines.first() else {
        return Err(MermaidRenderError::EmptyInput);
    };

    let mut header_tokens = header.split_whitespace();
    let keyword = header_tokens.next().unwrap_or("");
    let result = match keyword {
        "graph" | "flowchart" => {
            let direction = header_tokens.next().unwrap_or("TD").to_ascii_uppercase();
            if !matches!(direction.as_str(), "TD" | "TB" | "LR" | "RL" | "BT") {
                return Err(MermaidRenderError::SyntaxError {
                    line: header_line,
                    reason: format!("invalid flowchart direction '{direction}'"),
                });
            }
            let mut nodes: Vec<FlowNode> = Vec::new();
            let mut node_indices: std::collections::HashMap<String, usize> =
                std::collections::HashMap::new();
            let mut edges: Vec<FlowEdge> = Vec::new();

            let mut ensure_node =
                |id: String, label: Option<String>| -> Result<String, MermaidRenderError> {
                    if let Some(&idx) = node_indices.get(&id) {
                        if let Some(lbl) = label.filter(|l| !l.is_empty()) {
                            nodes[idx].label = lbl;
                        }
                        return Ok(id);
                    }
                    if nodes.len() >= options.max_nodes {
                        return Err(MermaidRenderError::NodeLimitExceeded {
                            nodes: nodes.len() + 1,
                            max_nodes: options.max_nodes,
                        });
                    }
                    let display = label
                        .filter(|l| !l.is_empty())
                        .unwrap_or_else(|| id.clone());
                    let idx = nodes.len();
                    nodes.push(FlowNode {
                        id: id.clone(),
                        label: display,
                    });
                    node_indices.insert(id.clone(), idx);
                    Ok(id)
                };

            for &(line_no, stmt) in &lines[1..] {
                if stmt.starts_with("subgraph ")
                    || stmt == "end"
                    || stmt.starts_with("classDef ")
                    || stmt.starts_with("class ")
                    || stmt.starts_with("style ")
                    || stmt.starts_with("linkStyle ")
                {
                    continue;
                }
                let mut rest = stmt;
                let mut parsed_edge = false;
                while let Some((lhs, label, rhs)) = split_mermaid_edge_once(rest) {
                    parsed_edge = true;
                    let (from_id, from_label) =
                        parse_mermaid_node_token(lhs).map_err(|reason| {
                            MermaidRenderError::SyntaxError {
                                line: line_no,
                                reason: reason.to_string(),
                            }
                        })?;
                    let from_key = ensure_node(from_id, from_label)?;

                    // Support chained edges like `A --> B --> C`.
                    let (rhs_head, next_rest) =
                        if let Some((next_lhs, _, _)) = split_mermaid_edge_once(rhs) {
                            (next_lhs, Some(rhs))
                        } else {
                            (rhs, None)
                        };
                    let (to_id, to_label) =
                        parse_mermaid_node_token(rhs_head).map_err(|reason| {
                            MermaidRenderError::SyntaxError {
                                line: line_no,
                                reason: reason.to_string(),
                            }
                        })?;
                    let to_key = ensure_node(to_id, to_label)?;
                    if edges.len() >= options.max_edges {
                        return Err(MermaidRenderError::EdgeLimitExceeded {
                            edges: edges.len() + 1,
                            max_edges: options.max_edges,
                        });
                    }
                    edges.push(FlowEdge {
                        from: from_key,
                        to: to_key,
                        label,
                    });
                    if let Some(tail) = next_rest {
                        rest = tail;
                    } else {
                        break;
                    }
                }
                if !parsed_edge {
                    let (id, label) = parse_mermaid_node_token(stmt).map_err(|reason| {
                        MermaidRenderError::SyntaxError {
                            line: line_no,
                            reason: reason.to_string(),
                        }
                    })?;
                    let _ = ensure_node(id, label)?;
                }
            }

            let rendered = render_flowchart_styled(&nodes, &edges, &direction, options.style);
            MermaidRenderResult {
                kind: MermaidDiagramKind::Flowchart,
                direction: Some(direction),
                node_count: nodes.len(),
                edge_count: edges.len(),
                rendered,
            }
        }
        "sequenceDiagram" => {
            let mut participants: Vec<(String, String)> = Vec::new();
            let mut messages: Vec<(String, String, String, bool)> = Vec::new();

            let mut ensure_participant =
                |id: String, alias: Option<String>| -> Result<(), MermaidRenderError> {
                    if let Some(entry) = participants.iter_mut().find(|(pid, _)| pid == &id) {
                        if let Some(a) = alias {
                            entry.1 = a;
                        }
                        return Ok(());
                    }
                    if participants.len() >= options.max_nodes {
                        return Err(MermaidRenderError::NodeLimitExceeded {
                            nodes: participants.len() + 1,
                            max_nodes: options.max_nodes,
                        });
                    }
                    let label = alias.unwrap_or_else(|| id.clone());
                    participants.push((id, label));
                    Ok(())
                };

            for &(line_no, stmt) in &lines[1..] {
                if let Some(rest) = stmt
                    .strip_prefix("participant ")
                    .or_else(|| stmt.strip_prefix("actor "))
                {
                    let rest = rest.trim();
                    if let Some((id, alias)) = rest.split_once(" as ") {
                        ensure_participant(id.trim().to_string(), Some(alias.trim().to_string()))?;
                    } else if !rest.is_empty() {
                        ensure_participant(rest.to_string(), None)?;
                    }
                    continue;
                }
                if stmt.starts_with("Note ") || stmt.starts_with("note ") {
                    continue;
                }
                let seq_ops: &[(&str, bool)] =
                    &[("-->>", true), ("->>", false), ("-->", true), ("->", false)];
                let mut matched = None;
                for &(op, dashed) in seq_ops {
                    if let Some(idx) = stmt.find(op) {
                        matched = Some((idx, op, dashed));
                        break;
                    }
                }
                let Some((idx, op, dashed)) = matched else {
                    return Err(MermaidRenderError::SyntaxError {
                        line: line_no,
                        reason: "expected participant declaration or message arrow".to_string(),
                    });
                };
                let from = stmt[..idx].trim();
                let rhs = stmt[idx + op.len()..].trim();
                let (to, msg) = rhs
                    .split_once(':')
                    .map_or((rhs.trim(), ""), |(t, m)| (t.trim(), m.trim()));
                if from.is_empty() || to.is_empty() {
                    return Err(MermaidRenderError::SyntaxError {
                        line: line_no,
                        reason: "sequence message missing sender or receiver".to_string(),
                    });
                }
                ensure_participant(from.to_string(), None)?;
                ensure_participant(to.to_string(), None)?;
                if messages.len() >= options.max_edges {
                    return Err(MermaidRenderError::EdgeLimitExceeded {
                        edges: messages.len() + 1,
                        max_edges: options.max_edges,
                    });
                }
                messages.push((from.to_string(), to.to_string(), msg.to_string(), dashed));
            }

            let rendered = render_sequence_diagram(&participants, &messages, options.style);
            MermaidRenderResult {
                kind: MermaidDiagramKind::Sequence,
                direction: None,
                node_count: participants.len(),
                edge_count: messages.len(),
                rendered,
            }
        }
        other => {
            return Err(MermaidRenderError::UnsupportedDiagramType {
                header: other.to_string(),
            });
        }
    };

    if result.rendered.len() > options.max_output_chars {
        return Err(MermaidRenderError::OutputTooLarge {
            chars: result.rendered.len(),
            max_chars: options.max_output_chars,
        });
    }
    Ok(result)
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
        assert_eq!(output, "");
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
            assert_ne!(output, "");
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

    // -- Bounded Mermaid parser & renderer (ft-xxfwy.54) --

    #[test]
    fn mermaid_flowchart_td_and_lr_render_labels_and_chains() {
        let src = r#"
            ```mermaid
            %% sample workflow
            flowchart TD
                A[Watcher] -->|captures| B(Storage) --> C{Policy}
            ```
        "#;
        let res = render_mermaid(src, &MermaidRenderOptions::default()).unwrap();
        assert_eq!(res.kind, MermaidDiagramKind::Flowchart);
        assert_eq!(res.direction.as_deref(), Some("TD"));
        assert_eq!(res.node_count, 3);
        assert_eq!(res.edge_count, 2);
        assert!(res.rendered.contains("Watcher"));
        assert!(res.rendered.contains("Storage"));
        assert!(res.rendered.contains("Policy"));
        assert!(res.rendered.contains("captures"));

        let lr = render_mermaid(
            "graph LR\n  A[Start] -->|go| B[Finish]\n",
            &MermaidRenderOptions {
                style: BoxStyle::Ascii,
                ..MermaidRenderOptions::default()
            },
        )
        .unwrap();
        assert_eq!(lr.direction.as_deref(), Some("LR"));
        assert!(lr.rendered.contains("[Start]"));
        assert!(lr.rendered.contains("[Finish]"));
        assert!(lr.rendered.contains("go"));
    }

    #[test]
    fn mermaid_sequence_diagram_renders_participants_and_messages() {
        let src = r#"
            sequenceDiagram
                participant CLI as ft CLI
                participant Mux as Mux Server
                CLI->>Mux: ListPanesWithDomainsV1
                Mux-->>CLI: 12 panes
        "#;
        let res = render_mermaid(src, &MermaidRenderOptions::default()).unwrap();
        assert_eq!(res.kind, MermaidDiagramKind::Sequence);
        assert_eq!(res.node_count, 2);
        assert_eq!(res.edge_count, 2);
        assert!(res.rendered.contains("ft CLI"));
        assert!(res.rendered.contains("Mux Server"));
        assert!(res.rendered.contains("ListPanesWithDomainsV1"));
        assert!(res.rendered.contains("12 panes"));
    }

    #[test]
    fn mermaid_rejects_empty_oversized_unsupported_and_limit_breaches() {
        assert_eq!(
            render_mermaid("   \n%% only comment\n", &MermaidRenderOptions::default()),
            Err(MermaidRenderError::EmptyInput)
        );
        assert_eq!(
            render_mermaid(
                "graph TD\nA --> B\n",
                &MermaidRenderOptions {
                    max_input_bytes: 8,
                    ..MermaidRenderOptions::default()
                }
            ),
            Err(MermaidRenderError::InputTooLarge {
                bytes: 17,
                max_bytes: 8,
            })
        );
        assert_eq!(
            render_mermaid("pie\n  \"A\" : 10\n", &MermaidRenderOptions::default()),
            Err(MermaidRenderError::UnsupportedDiagramType {
                header: "pie".to_string(),
            })
        );
        assert_eq!(
            render_mermaid(
                "flowchart TD\nA --> B\nB --> C\n",
                &MermaidRenderOptions {
                    max_nodes: 2,
                    ..MermaidRenderOptions::default()
                }
            ),
            Err(MermaidRenderError::NodeLimitExceeded {
                nodes: 3,
                max_nodes: 2,
            })
        );
        assert_eq!(
            render_mermaid(
                "flowchart TD\nA --> B\nB --> C\n",
                &MermaidRenderOptions {
                    max_edges: 1,
                    ..MermaidRenderOptions::default()
                }
            ),
            Err(MermaidRenderError::EdgeLimitExceeded {
                edges: 2,
                max_edges: 1,
            })
        );
    }
}
