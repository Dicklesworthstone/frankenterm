//! br-ft-1650n.16: Operator Causal Graph console renderer.
//!
//! Text-first operator UI for the bounded per-session causal
//! graph (`crate::explainability_console::CausalGraphNode` +
//! `CausalGraphEdge`). The bead's "Start with robot/TOON and
//! textual tree output before GUI polish" item lives here.
//!
//! Renderer is pure: same input always produces the same output.
//! The wired-pass slice will pull `CausalGraphNode`/`Edge`
//! collections from the live graph ledger and feed them in.
//!
//! ## What ships in this slice
//!
//! - [`FocusOptions`] — operator-facing filters: pane_id,
//!   time range, low-confidence threshold.
//! - [`UncertaintyMarker`] — typed confidence band (`Confident`
//!   / `LowConfidence` / `Uncertain`) derived from
//!   `CausalGraphEdge::confidence_bps`.
//! - [`render_timeline_text`] — chronological list of nodes
//!   (timeline-scrub equivalent) with optional pane focus
//!   filter and uncertainty markers on inbound edges.
//! - [`render_dependency_tree`] — text dependency tree rooted
//!   at a focus node (operator picks a node id; the renderer
//!   walks ancestors / descendants).
//! - [`render_evidence_snippets`] — per-node redacted evidence
//!   strings (label + context map). Trusts the substrate's
//!   redaction contract (context values are already redacted).
//! - [`render_robot_toon`] — TOON-style key:value output for
//!   robot/automation consumers.
//!
//! ## What is deferred
//!
//! - GUI surface: bead's "Add GUI view only after the schema
//!   and query surfaces are stable" — this slice ships the
//!   text/TOON path; GUI is a follow-up.
//! - Causal-graph schema migrations: substrate uses the
//!   existing `CausalGraphNode`/`Edge` shape.
//! - Evidence hyperlinks: today rendered as plain text; the
//!   wired-pass slice can add source-id-to-pane-link mapping.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::explainability_console::{
    CausalEvidenceKind, CausalGraphEdge, CausalGraphNode, CausalNodeKind,
    CausalTraversalDirection,
};

/// Stable schema version for `CausalConsoleRendering` exports.
pub const CAUSAL_CONSOLE_RENDERING_SCHEMA: &str = "ft.causal_console.rendering.v1";

/// br-ft-1650n.16: operator-facing filters. The renderer
/// applies these before laying out the output.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct FocusOptions {
    /// Restrict to nodes touching this pane.
    pub pane_id: Option<u64>,
    /// Restrict to nodes in this time window (inclusive).
    pub start_ms: Option<u64>,
    pub end_ms: Option<u64>,
    /// Confidence threshold (basis points). Edges below this get
    /// flagged with `UncertaintyMarker::LowConfidence`. Default
    /// is 5_000 (50%).
    pub low_confidence_bps_threshold: Option<u16>,
    /// Restrict to nodes of these kinds. Empty = all kinds.
    pub kinds: Vec<CausalNodeKind>,
}

impl FocusOptions {
    fn low_confidence_threshold(&self) -> u16 {
        self.low_confidence_bps_threshold.unwrap_or(5_000)
    }

    fn matches(&self, node: &CausalGraphNode) -> bool {
        if let Some(p) = self.pane_id {
            if node.pane_id != Some(p) {
                return false;
            }
        }
        if let Some(start) = self.start_ms {
            if node.timestamp_ms < start {
                return false;
            }
        }
        if let Some(end) = self.end_ms {
            if node.timestamp_ms > end {
                return false;
            }
        }
        if !self.kinds.is_empty() && !self.kinds.contains(&node.kind) {
            return false;
        }
        true
    }
}

/// br-ft-1650n.16: typed confidence band derived from an edge's
/// `confidence_bps`. Operators read this on the timeline so a
/// dense incident graph still reads at a glance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UncertaintyMarker {
    /// confidence_bps ≥ 8_000 (80%).
    Confident,
    /// 5_000 ≤ confidence_bps < 8_000.
    LowConfidence,
    /// confidence_bps < threshold (default 5_000).
    Uncertain,
}

impl UncertaintyMarker {
    /// Glyph used in the text rendering. ASCII-only so logs
    /// scrub cleanly.
    fn glyph(self) -> &'static str {
        match self {
            Self::Confident => "[+]",
            Self::LowConfidence => "[?]",
            Self::Uncertain => "[??]",
        }
    }

    fn classify(confidence_bps: u16, threshold_bps: u16) -> Self {
        if confidence_bps >= 8_000 {
            Self::Confident
        } else if confidence_bps >= threshold_bps {
            Self::LowConfidence
        } else {
            Self::Uncertain
        }
    }
}

/// br-ft-1650n.16: structured rendering for machine consumers
/// (TOON / JSON automation). Mirrors the text rendering shape so
/// snapshot tests can pin both at once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CausalConsoleRendering {
    pub schema_version: String,
    pub timeline: Vec<TimelineEntry>,
    pub uncertain_edges: Vec<UncertainEdgeEntry>,
    pub evidence_snippets: Vec<EvidenceSnippet>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineEntry {
    pub node_id: String,
    pub timestamp_ms: u64,
    pub kind: CausalNodeKind,
    pub pane_id: Option<u64>,
    pub label: String,
    pub inbound_uncertainty: Option<UncertaintyMarker>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UncertainEdgeEntry {
    pub from: String,
    pub to: String,
    pub confidence_bps: u16,
    pub marker: UncertaintyMarker,
    pub evidence: CausalEvidenceKind,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceSnippet {
    pub node_id: String,
    pub label: String,
    pub context: BTreeMap<String, String>,
}

/// br-ft-1650n.16: timeline-scrub equivalent in plain text.
/// Each line: `[timestamp] [pane:N] [marker] kind: label`.
/// Sorted ascending by `timestamp_ms`; ties broken by node id
/// for determinism.
#[must_use]
pub fn render_timeline_text(
    nodes: &[CausalGraphNode],
    edges: &[CausalGraphEdge],
    options: &FocusOptions,
) -> String {
    let mut filtered: Vec<&CausalGraphNode> =
        nodes.iter().filter(|n| options.matches(n)).collect();
    filtered.sort_by(|a, b| {
        a.timestamp_ms
            .cmp(&b.timestamp_ms)
            .then_with(|| a.id.cmp(&b.id))
    });

    let inbound_marker = inbound_marker_map(edges, options.low_confidence_threshold());

    let mut out = String::new();
    out.push_str("=== causal graph timeline ===\n");
    if let Some(p) = options.pane_id {
        out.push_str(&format!("focus: pane={p}\n"));
    }
    if filtered.is_empty() {
        out.push_str("(no nodes match the focus filter)\n");
        return out;
    }
    for n in &filtered {
        let pane_label = n
            .pane_id
            .map(|p| format!("pane:{p} "))
            .unwrap_or_default();
        let marker = inbound_marker
            .get(&n.id)
            .copied()
            .map(|m| format!("{} ", m.glyph()))
            .unwrap_or_default();
        out.push_str(&format!(
            "[{ts}] {pane}{marker}{kind:?}: {label}\n",
            ts = n.timestamp_ms,
            pane = pane_label,
            marker = marker,
            kind = n.kind,
            label = n.label,
        ));
    }
    out
}

/// Map node_id → "worst" inbound uncertainty marker. Used by
/// the timeline view to flag nodes that hang off low-confidence
/// edges.
fn inbound_marker_map(
    edges: &[CausalGraphEdge],
    threshold_bps: u16,
) -> HashMap<String, UncertaintyMarker> {
    let mut out: HashMap<String, UncertaintyMarker> = HashMap::new();
    for e in edges {
        let marker = UncertaintyMarker::classify(e.confidence_bps, threshold_bps);
        let entry = out.entry(e.to.clone()).or_insert(UncertaintyMarker::Confident);
        // Promote to the LEAST confident marker we've seen on
        // any inbound edge (Uncertain > LowConfidence > Confident).
        *entry = worst_of(*entry, marker);
    }
    out
}

fn worst_of(a: UncertaintyMarker, b: UncertaintyMarker) -> UncertaintyMarker {
    use UncertaintyMarker::*;
    match (a, b) {
        (Uncertain, _) | (_, Uncertain) => Uncertain,
        (LowConfidence, _) | (_, LowConfidence) => LowConfidence,
        _ => Confident,
    }
}

/// br-ft-1650n.16: dependency tree from a focus node. Walks
/// either ancestors or descendants up to `max_depth` levels.
/// Indents per level. Uncertainty markers on each traversed
/// edge.
#[must_use]
pub fn render_dependency_tree(
    nodes: &[CausalGraphNode],
    edges: &[CausalGraphEdge],
    focus_node_id: &str,
    direction: CausalTraversalDirection,
    max_depth: u32,
    threshold_bps: u16,
) -> String {
    let node_index: HashMap<&str, &CausalGraphNode> =
        nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    let mut out = String::new();
    let dir_label = match direction {
        CausalTraversalDirection::Ancestors => "ancestors",
        CausalTraversalDirection::Descendants => "descendants",
    };
    out.push_str(&format!(
        "=== causal {dir_label} tree (focus={focus_node_id}, max_depth={max_depth}) ===\n"
    ));
    if !node_index.contains_key(focus_node_id) {
        out.push_str("(focus node not found in input)\n");
        return out;
    }

    let mut visited: HashSet<String> = HashSet::new();
    walk(
        focus_node_id,
        edges,
        &node_index,
        direction,
        max_depth,
        threshold_bps,
        0,
        &mut visited,
        None,
        &mut out,
    );
    out
}

#[allow(clippy::too_many_arguments)]
fn walk(
    cursor: &str,
    edges: &[CausalGraphEdge],
    node_index: &HashMap<&str, &CausalGraphNode>,
    direction: CausalTraversalDirection,
    max_depth: u32,
    threshold_bps: u16,
    depth: u32,
    visited: &mut HashSet<String>,
    edge_marker: Option<UncertaintyMarker>,
    out: &mut String,
) {
    if !visited.insert(cursor.to_string()) {
        return;
    }
    let indent = "  ".repeat(depth as usize);
    let marker_label = edge_marker
        .map(|m| format!("{} ", m.glyph()))
        .unwrap_or_default();
    if let Some(node) = node_index.get(cursor) {
        out.push_str(&format!(
            "{indent}{marker}[{ts}] {kind:?}: {label} ({id})\n",
            indent = indent,
            marker = marker_label,
            ts = node.timestamp_ms,
            kind = node.kind,
            label = node.label,
            id = node.id,
        ));
    } else {
        out.push_str(&format!("{indent}{marker_label}(missing node {cursor})\n"));
    }
    if depth >= max_depth {
        return;
    }
    for e in edges {
        let next = match direction {
            CausalTraversalDirection::Ancestors if e.to == cursor => &e.from,
            CausalTraversalDirection::Descendants if e.from == cursor => &e.to,
            _ => continue,
        };
        let m = UncertaintyMarker::classify(e.confidence_bps, threshold_bps);
        walk(
            next,
            edges,
            node_index,
            direction,
            max_depth,
            threshold_bps,
            depth + 1,
            visited,
            Some(m),
            out,
        );
    }
}

/// br-ft-1650n.16: per-node redacted evidence snippets. Output
/// is ordered ascending by `timestamp_ms` for determinism.
#[must_use]
pub fn render_evidence_snippets(
    nodes: &[CausalGraphNode],
    options: &FocusOptions,
) -> Vec<EvidenceSnippet> {
    let mut filtered: Vec<&CausalGraphNode> =
        nodes.iter().filter(|n| options.matches(n)).collect();
    filtered.sort_by(|a, b| {
        a.timestamp_ms
            .cmp(&b.timestamp_ms)
            .then_with(|| a.id.cmp(&b.id))
    });
    filtered
        .into_iter()
        .map(|n| EvidenceSnippet {
            node_id: n.id.clone(),
            label: n.label.clone(),
            context: n.context.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        })
        .collect()
}

/// br-ft-1650n.16: structured TOON-friendly rendering.
#[must_use]
pub fn render_robot_toon(
    nodes: &[CausalGraphNode],
    edges: &[CausalGraphEdge],
    options: &FocusOptions,
) -> CausalConsoleRendering {
    let threshold_bps = options.low_confidence_threshold();
    let inbound_marker = inbound_marker_map(edges, threshold_bps);

    let mut filtered: Vec<&CausalGraphNode> =
        nodes.iter().filter(|n| options.matches(n)).collect();
    filtered.sort_by(|a, b| {
        a.timestamp_ms
            .cmp(&b.timestamp_ms)
            .then_with(|| a.id.cmp(&b.id))
    });
    let timeline: Vec<TimelineEntry> = filtered
        .iter()
        .map(|n| TimelineEntry {
            node_id: n.id.clone(),
            timestamp_ms: n.timestamp_ms,
            kind: n.kind,
            pane_id: n.pane_id,
            label: n.label.clone(),
            inbound_uncertainty: inbound_marker.get(&n.id).copied(),
        })
        .collect();

    let mut uncertain_edges: Vec<UncertainEdgeEntry> = edges
        .iter()
        .filter(|e| e.confidence_bps < 8_000)
        .map(|e| UncertainEdgeEntry {
            from: e.from.clone(),
            to: e.to.clone(),
            confidence_bps: e.confidence_bps,
            marker: UncertaintyMarker::classify(e.confidence_bps, threshold_bps),
            evidence: e.evidence,
            source: e.source.clone(),
        })
        .collect();
    uncertain_edges.sort_by(|a, b| {
        a.from
            .cmp(&b.from)
            .then_with(|| a.to.cmp(&b.to))
            .then_with(|| a.confidence_bps.cmp(&b.confidence_bps))
    });

    let evidence_snippets = render_evidence_snippets(nodes, options);

    CausalConsoleRendering {
        schema_version: CAUSAL_CONSOLE_RENDERING_SCHEMA.to_string(),
        timeline,
        uncertain_edges,
        evidence_snippets,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, kind: CausalNodeKind, ts: u64, label: &str) -> CausalGraphNode {
        CausalGraphNode::new(id, kind, ts, label)
    }

    fn node_with_pane(
        id: &str,
        kind: CausalNodeKind,
        ts: u64,
        label: &str,
        pane: u64,
    ) -> CausalGraphNode {
        CausalGraphNode::new(id, kind, ts, label).with_pane_id(pane)
    }

    fn edge(from: &str, to: &str, confidence_bps: u16) -> CausalGraphEdge {
        CausalGraphEdge::new(from, to, CausalEvidenceKind::Observed, confidence_bps, "test")
    }

    /// Empty nodes input renders the timeline header + a
    /// "no nodes match" footer.
    #[test]
    fn empty_timeline_emits_header_and_footer() {
        let text = render_timeline_text(&[], &[], &FocusOptions::default());
        assert!(text.starts_with("=== causal graph timeline ==="));
        assert!(text.contains("(no nodes match"));
    }

    /// Timeline orders nodes ascending by timestamp.
    #[test]
    fn timeline_orders_by_timestamp_ascending() {
        let nodes = vec![
            node("c", CausalNodeKind::PaneEvent, 30, "third"),
            node("a", CausalNodeKind::PaneEvent, 10, "first"),
            node("b", CausalNodeKind::PaneEvent, 20, "second"),
        ];
        let text = render_timeline_text(&nodes, &[], &FocusOptions::default());
        let first_idx = text.find("first").expect("first present");
        let second_idx = text.find("second").expect("second present");
        let third_idx = text.find("third").expect("third present");
        assert!(first_idx < second_idx);
        assert!(second_idx < third_idx);
    }

    /// Pane focus filter restricts the timeline to nodes touching
    /// the chosen pane.
    #[test]
    fn pane_focus_filter_restricts_timeline() {
        let nodes = vec![
            node_with_pane("a", CausalNodeKind::PaneEvent, 10, "pane_1_event", 1),
            node_with_pane("b", CausalNodeKind::PaneEvent, 20, "pane_2_event", 2),
        ];
        let opts = FocusOptions {
            pane_id: Some(1),
            ..Default::default()
        };
        let text = render_timeline_text(&nodes, &[], &opts);
        assert!(text.contains("pane_1_event"));
        assert!(!text.contains("pane_2_event"));
        assert!(text.contains("focus: pane=1"));
    }

    /// Time-range filter excludes out-of-range nodes.
    #[test]
    fn time_range_filter_excludes_out_of_range() {
        let nodes = vec![
            node("a", CausalNodeKind::PaneEvent, 5, "early"),
            node("b", CausalNodeKind::PaneEvent, 15, "in_range"),
            node("c", CausalNodeKind::PaneEvent, 25, "late"),
        ];
        let opts = FocusOptions {
            start_ms: Some(10),
            end_ms: Some(20),
            ..Default::default()
        };
        let text = render_timeline_text(&nodes, &[], &opts);
        assert!(text.contains("in_range"));
        assert!(!text.contains("early"));
        assert!(!text.contains("late"));
    }

    /// Kind filter restricts to a subset.
    #[test]
    fn kind_filter_restricts_subset() {
        let nodes = vec![
            node("a", CausalNodeKind::PaneEvent, 10, "raw_event"),
            node("b", CausalNodeKind::PolicyDecision, 20, "policy_decision"),
        ];
        let opts = FocusOptions {
            kinds: vec![CausalNodeKind::PolicyDecision],
            ..Default::default()
        };
        let text = render_timeline_text(&nodes, &[], &opts);
        assert!(text.contains("policy_decision"));
        assert!(!text.contains("raw_event"));
    }

    /// Uncertainty markers: an inbound edge with low confidence
    /// flags the destination node in the timeline.
    #[test]
    fn timeline_marks_low_confidence_inbound_edges() {
        let nodes = vec![
            node("a", CausalNodeKind::PaneEvent, 10, "src"),
            node("b", CausalNodeKind::PolicyDecision, 20, "dst"),
        ];
        let edges = vec![edge("a", "b", 3_000)]; // < 5_000 default
        let text = render_timeline_text(&nodes, &edges, &FocusOptions::default());
        // Destination node carries the [??] marker.
        assert!(text.contains("[??]"));
    }

    /// UncertaintyMarker classification thresholds: ≥8_000 is
    /// Confident, 5_000-8_000 LowConfidence, <5_000 Uncertain.
    #[test]
    fn uncertainty_classification_thresholds() {
        assert_eq!(
            UncertaintyMarker::classify(9_000, 5_000),
            UncertaintyMarker::Confident
        );
        assert_eq!(
            UncertaintyMarker::classify(8_000, 5_000),
            UncertaintyMarker::Confident
        );
        assert_eq!(
            UncertaintyMarker::classify(6_000, 5_000),
            UncertaintyMarker::LowConfidence
        );
        assert_eq!(
            UncertaintyMarker::classify(5_000, 5_000),
            UncertaintyMarker::LowConfidence
        );
        assert_eq!(
            UncertaintyMarker::classify(2_000, 5_000),
            UncertaintyMarker::Uncertain
        );
    }

    /// Worst-of: two markers reduce to the least confident.
    #[test]
    fn worst_of_picks_least_confident() {
        use UncertaintyMarker::*;
        assert_eq!(worst_of(Confident, LowConfidence), LowConfidence);
        assert_eq!(worst_of(LowConfidence, Uncertain), Uncertain);
        assert_eq!(worst_of(Confident, Uncertain), Uncertain);
        assert_eq!(worst_of(Confident, Confident), Confident);
    }

    /// Dependency tree (descendants) walks outgoing edges from
    /// the focus node up to max_depth.
    #[test]
    fn dependency_tree_descendants_walks_outgoing_edges() {
        let nodes = vec![
            node("root", CausalNodeKind::PaneEvent, 10, "root"),
            node("c1", CausalNodeKind::PolicyDecision, 20, "child1"),
            node("c2", CausalNodeKind::WorkflowTrigger, 30, "child2"),
            node("g1", CausalNodeKind::Approval, 40, "grandchild1"),
        ];
        let edges = vec![
            edge("root", "c1", 9_000),
            edge("root", "c2", 9_000),
            edge("c1", "g1", 9_000),
        ];
        let text = render_dependency_tree(
            &nodes,
            &edges,
            "root",
            CausalTraversalDirection::Descendants,
            3,
            5_000,
        );
        assert!(text.contains("descendants"));
        assert!(text.contains("root"));
        assert!(text.contains("child1"));
        assert!(text.contains("child2"));
        assert!(text.contains("grandchild1"));
    }

    /// Dependency tree (ancestors) walks incoming edges.
    #[test]
    fn dependency_tree_ancestors_walks_incoming_edges() {
        let nodes = vec![
            node("root", CausalNodeKind::PaneEvent, 10, "root_cause"),
            node("mid", CausalNodeKind::PolicyDecision, 20, "intermediate"),
            node("leaf", CausalNodeKind::Approval, 30, "leaf_event"),
        ];
        let edges = vec![edge("root", "mid", 9_000), edge("mid", "leaf", 9_000)];
        let text = render_dependency_tree(
            &nodes,
            &edges,
            "leaf",
            CausalTraversalDirection::Ancestors,
            3,
            5_000,
        );
        assert!(text.contains("leaf_event"));
        assert!(text.contains("intermediate"));
        assert!(text.contains("root_cause"));
    }

    /// Dependency tree honors `max_depth`: a depth of 1 includes
    /// only the focus node + its immediate neighbors.
    #[test]
    fn dependency_tree_respects_max_depth() {
        let nodes = vec![
            node("root", CausalNodeKind::PaneEvent, 10, "root"),
            node("c1", CausalNodeKind::PaneEvent, 20, "child"),
            node("g1", CausalNodeKind::PaneEvent, 30, "grandchild"),
        ];
        let edges = vec![edge("root", "c1", 9_000), edge("c1", "g1", 9_000)];
        let text = render_dependency_tree(
            &nodes,
            &edges,
            "root",
            CausalTraversalDirection::Descendants,
            1,
            5_000,
        );
        assert!(text.contains("root"));
        assert!(text.contains("child"));
        assert!(!text.contains("grandchild"));
    }

    /// Missing focus node renders an explicit fallback message.
    #[test]
    fn dependency_tree_handles_missing_focus_node() {
        let text = render_dependency_tree(
            &[],
            &[],
            "missing",
            CausalTraversalDirection::Descendants,
            3,
            5_000,
        );
        assert!(text.contains("focus node not found"));
    }

    /// Cycle handling: walk does not infinite-loop on a graph
    /// with a cycle.
    #[test]
    fn dependency_tree_handles_cycles() {
        let nodes = vec![
            node("a", CausalNodeKind::PaneEvent, 10, "a"),
            node("b", CausalNodeKind::PaneEvent, 20, "b"),
        ];
        let edges = vec![edge("a", "b", 9_000), edge("b", "a", 9_000)];
        let text = render_dependency_tree(
            &nodes,
            &edges,
            "a",
            CausalTraversalDirection::Descendants,
            10,
            5_000,
        );
        // Each node appears at most once (visited set guards).
        let a_count = text.matches("[10]").count();
        let b_count = text.matches("[20]").count();
        assert_eq!(a_count, 1);
        assert_eq!(b_count, 1);
    }

    /// Evidence snippets preserve already-redacted context map.
    /// The substrate trusts the context values are redacted.
    #[test]
    fn evidence_snippets_preserve_context() {
        let mut n = node("a", CausalNodeKind::PaneEvent, 10, "label");
        n.context.insert("k1".to_string(), "v1".to_string());
        n.context.insert("k2".to_string(), "[REDACTED]".to_string());
        let snippets = render_evidence_snippets(&[n], &FocusOptions::default());
        assert_eq!(snippets.len(), 1);
        assert_eq!(snippets[0].context.get("k1"), Some(&"v1".to_string()));
        assert_eq!(
            snippets[0].context.get("k2"),
            Some(&"[REDACTED]".to_string())
        );
    }

    /// TOON rendering captures timeline + uncertain_edges +
    /// evidence_snippets and pins the schema version.
    #[test]
    fn robot_toon_rendering_has_three_sections() {
        let nodes = vec![
            node_with_pane("a", CausalNodeKind::PaneEvent, 10, "src", 1),
            node("b", CausalNodeKind::PolicyDecision, 20, "dst"),
        ];
        let edges = vec![edge("a", "b", 3_000)];
        let r = render_robot_toon(&nodes, &edges, &FocusOptions::default());
        assert_eq!(r.schema_version, CAUSAL_CONSOLE_RENDERING_SCHEMA);
        assert_eq!(r.timeline.len(), 2);
        // Edge with 3_000 confidence_bps lands in uncertain_edges
        // (both LowConfidence and Uncertain count as < 8_000).
        assert_eq!(r.uncertain_edges.len(), 1);
        assert_eq!(r.uncertain_edges[0].marker, UncertaintyMarker::Uncertain);
        assert_eq!(r.evidence_snippets.len(), 2);
    }

    /// TOON rendering serde roundtrip.
    #[test]
    fn rendering_serde_roundtrip() {
        let nodes = vec![node_with_pane("a", CausalNodeKind::PaneEvent, 10, "x", 1)];
        let r = render_robot_toon(&nodes, &[], &FocusOptions::default());
        let json = serde_json::to_string(&r).expect("serialize");
        let back: CausalConsoleRendering = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(r, back);
    }

    /// Pure function: same input always yields same output.
    #[test]
    fn render_text_is_pure() {
        let nodes = vec![node("a", CausalNodeKind::PaneEvent, 10, "x")];
        let t1 = render_timeline_text(&nodes, &[], &FocusOptions::default());
        let t2 = render_timeline_text(&nodes, &[], &FocusOptions::default());
        assert_eq!(t1, t2);
    }

    /// Determinism: tied timestamps break by node id ascending.
    #[test]
    fn timeline_ties_break_by_node_id() {
        let nodes = vec![
            node("z", CausalNodeKind::PaneEvent, 10, "z_label"),
            node("a", CausalNodeKind::PaneEvent, 10, "a_label"),
        ];
        let text = render_timeline_text(&nodes, &[], &FocusOptions::default());
        let a_idx = text.find("a_label").expect("present");
        let z_idx = text.find("z_label").expect("present");
        assert!(a_idx < z_idx, "ties must break by node id ascending");
    }

    /// Snapshot test for representative incident graph (the
    /// bead's "Snapshot tests for text/TOON output" criterion).
    /// Pinned to a stable string so any drift in the output is
    /// visible in the diff.
    #[test]
    fn snapshot_representative_incident_graph() {
        let nodes = vec![
            node_with_pane("evt-1", CausalNodeKind::PaneEvent, 1_000, "ssh failure", 1),
            node_with_pane(
                "pat-1",
                CausalNodeKind::PatternMatch,
                1_100,
                "rate-limit pattern",
                1,
            ),
            node("dec-1", CausalNodeKind::PolicyDecision, 1_200, "deny"),
        ];
        let edges = vec![
            edge("evt-1", "pat-1", 9_000),
            edge("pat-1", "dec-1", 4_000),
        ];
        let text = render_timeline_text(&nodes, &edges, &FocusOptions::default());
        let expected = "=== causal graph timeline ===\n\
[1000] pane:1 PaneEvent: ssh failure\n\
[1100] pane:1 [+] PatternMatch: rate-limit pattern\n\
[1200] [??] PolicyDecision: deny\n";
        assert_eq!(text, expected);
    }
}
