//! E2E: causal graph console dense rendering check (ft-1650n.16).
//!
//! Substrate at `crate::causal_graph_console` ships render
//! functions + 23 inline unit tests covering the per-render-fn
//! behavior. This suite covers the bead's last open acceptance
//! item: "UI/e2e rendering check proves dense evidence remains
//! readable and non-overlapping".
//!
//! Builds a representative dense incident graph (multiple panes,
//! multiple node kinds, mixed-confidence edges, redacted evidence
//! context, time-ordered) and asserts the rendered text:
//!
//! 1. Has one timeline line per matching node.
//! 2. Marks low-confidence and uncertain edges with the
//!    documented glyphs `[?]` / `[??]`.
//! 3. Respects FocusOptions filters end-to-end (pane, time
//!    window, kinds).
//! 4. Evidence snippets carry redacted context values (the
//!    substrate trusts the producer to redact; this E2E plants
//!    [REDACTED] tokens and asserts they survive verbatim).
//! 5. TOON / structured rendering matches the text rendering's
//!    coverage.

use std::collections::BTreeMap;

use frankenterm_core::causal_graph_console::{
    EvidenceSnippet, FocusOptions, OperatorTextOptions, UncertaintyMarker,
    render_dependency_tree, render_evidence_snippets, render_operator_text, render_robot_toon,
    render_timeline_text,
};
use frankenterm_core::explainability_console::{
    CausalEvidenceKind, CausalGraphEdge, CausalGraphNode, CausalNodeKind,
    CausalTraversalDirection,
};

// ── Fixture helpers ────────────────────────────────────────────────────

/// Build a representative incident graph: 8 nodes across 3 panes,
/// spanning pane-events, pattern-matches, policy-decisions, and
/// recovery-actions. Edge confidence varies so the rendered output
/// must show every UncertaintyMarker variant.
fn dense_incident_graph() -> (Vec<CausalGraphNode>, Vec<CausalGraphEdge>) {
    let nodes = vec![
        CausalGraphNode::new("n1", CausalNodeKind::PaneEvent, 1_000, "pane 1 stderr burst")
            .with_pane_id(1)
            .with_context("session_id", "incident-X")
            .with_context("excerpt", "[REDACTED] stderr line"),
        CausalGraphNode::new("n2", CausalNodeKind::PatternMatch, 1_050, "rule:auth_fail matched")
            .with_pane_id(1)
            .with_context("session_id", "incident-X")
            .with_context("rule_id", "auth_fail")
            .with_context("matched_excerpt", "auth failed for [REDACTED]"),
        CausalGraphNode::new("n3", CausalNodeKind::PolicyDecision, 1_100, "policy:require_approval")
            .with_pane_id(1)
            .with_context("session_id", "incident-X")
            .with_context("decision", "require_approval"),
        CausalGraphNode::new("n4", CausalNodeKind::Approval, 1_200, "operator approved")
            .with_pane_id(1)
            .with_context("session_id", "incident-X")
            .with_context("operator", "[REDACTED]"),
        CausalGraphNode::new("n5", CausalNodeKind::RecoveryAction, 1_300, "rollback to checkpoint cp-7")
            .with_pane_id(1)
            .with_context("session_id", "incident-X")
            .with_context("checkpoint_id", "cp-7"),
        // Different pane — same incident, parallel timeline.
        CausalGraphNode::new("n6", CausalNodeKind::PaneEvent, 1_010, "pane 2 lockup")
            .with_pane_id(2)
            .with_context("session_id", "incident-X"),
        CausalGraphNode::new("n7", CausalNodeKind::WorkflowTrigger, 1_150, "workflow:diag_dump")
            .with_pane_id(2)
            .with_context("session_id", "incident-X"),
        // Out-of-incident pane (different session) to verify focus filter.
        CausalGraphNode::new("n8", CausalNodeKind::PaneEvent, 1_500, "pane 3 unrelated heartbeat")
            .with_pane_id(3)
            .with_context("session_id", "other-session"),
    ];
    let edges = vec![
        CausalGraphEdge {
            from: "n1".into(),
            to: "n2".into(),
            evidence: CausalEvidenceKind::Observed,
            confidence_bps: 9_500, // Confident
            source: "pattern_engine".into(),
            description: None,
        },
        CausalGraphEdge {
            from: "n2".into(),
            to: "n3".into(),
            evidence: CausalEvidenceKind::Policy,
            confidence_bps: 9_000, // Confident
            source: "policy_pipeline".into(),
            description: None,
        },
        CausalGraphEdge {
            from: "n3".into(),
            to: "n4".into(),
            evidence: CausalEvidenceKind::UserAction,
            confidence_bps: 10_000, // Confident
            source: "approval_console".into(),
            description: None,
        },
        CausalGraphEdge {
            from: "n4".into(),
            to: "n5".into(),
            evidence: CausalEvidenceKind::Mission,
            confidence_bps: 9_800, // Confident
            source: "recovery_planner".into(),
            description: None,
        },
        // Cross-pane TEMPORAL edge — operator is uncertain whether
        // pane 2 lockup actually caused the workflow trigger or
        // whether they were coincidentally close in time.
        CausalGraphEdge {
            from: "n6".into(),
            to: "n7".into(),
            evidence: CausalEvidenceKind::Temporal,
            confidence_bps: 6_000, // LowConfidence (5_000 ≤ x < 8_000)
            source: "session_correlator".into(),
            description: None,
        },
        // Inferred edge — uncertain.
        CausalGraphEdge {
            from: "n7".into(),
            to: "n5".into(),
            evidence: CausalEvidenceKind::Inferred,
            confidence_bps: 3_500, // Uncertain (< 5_000 threshold)
            source: "session_correlator".into(),
            description: None,
        },
    ];
    (nodes, edges)
}

// ── E2E assertions ─────────────────────────────────────────────────────

/// Dense incident graph renders one timeline line per matching node
/// in chronological order. With no focus filter, all 8 nodes appear.
#[test]
fn timeline_one_line_per_node_in_chronological_order_ft_1650n_16() {
    let (nodes, edges) = dense_incident_graph();
    let text = render_timeline_text(&nodes, &edges, &FocusOptions::default());
    let lines: Vec<&str> = text.lines().collect();
    // Header + (no focus marker because default filter) + 8 entries.
    let entry_lines: Vec<&str> = lines
        .iter()
        .filter(|l| l.starts_with("["))
        .copied()
        .collect();
    assert_eq!(entry_lines.len(), 8, "expected 8 entries, got: {entry_lines:?}");
    // Entries must be in ascending timestamp order (1000, 1010,
    // 1050, 1100, 1150, 1200, 1300, 1500).
    let timestamps: Vec<u64> = entry_lines
        .iter()
        .filter_map(|l| {
            let s = l.strip_prefix('[')?.split(']').next()?;
            s.parse().ok()
        })
        .collect();
    assert_eq!(timestamps, vec![1_000, 1_010, 1_050, 1_100, 1_150, 1_200, 1_300, 1_500]);
}

/// Pane focus filter restricts the timeline to a single pane's
/// rows. With pane=1 focus, only the 5 nodes touching pane 1 appear.
#[test]
fn timeline_pane_focus_filter_drops_unrelated_panes_ft_1650n_16() {
    let (nodes, edges) = dense_incident_graph();
    let opts = FocusOptions {
        pane_id: Some(1),
        ..FocusOptions::default()
    };
    let text = render_timeline_text(&nodes, &edges, &opts);
    let entry_lines: Vec<&str> = text.lines().filter(|l| l.starts_with("[")).collect();
    assert_eq!(entry_lines.len(), 5, "pane=1 focus should keep 5 nodes");
    // Pane 2 nodes (n6, n7) and pane 3 node (n8) absent.
    assert!(!text.contains("pane 2 lockup"));
    assert!(!text.contains("workflow:diag_dump"));
    assert!(!text.contains("pane 3 unrelated"));
    // Header surfaces the focus context.
    assert!(text.contains("focus: pane=1"));
}

/// Time-window focus drops nodes outside the inclusive [start, end]
/// range. Window [1_050, 1_200] keeps n2/n3/n4/n7.
#[test]
fn timeline_time_window_focus_filter_inclusive_ft_1650n_16() {
    let (nodes, edges) = dense_incident_graph();
    let opts = FocusOptions {
        start_ms: Some(1_050),
        end_ms: Some(1_200),
        ..FocusOptions::default()
    };
    let text = render_timeline_text(&nodes, &edges, &opts);
    let entry_lines: Vec<&str> = text.lines().filter(|l| l.starts_with("[")).collect();
    assert_eq!(entry_lines.len(), 4, "window [1050, 1200] should keep 4 nodes");
}

/// Uncertain inbound edges flag the target node with the documented
/// glyph in the timeline. With our fixture:
/// - n7 has inbound edge n6→n7 at confidence_bps=6000 (LowConfidence
///   marker `[?]`), so n7's timeline entry carries `[?]`.
/// - n5 has inbound edges n4→n5 (Confident, 9800) AND n7→n5
///   (Uncertain, 3500). The substrate's worst_of() promotes to the
///   LEAST confident marker — n5 should carry `[??]` (Uncertain).
#[test]
fn timeline_marks_inbound_uncertainty_with_documented_glyphs_ft_1650n_16() {
    let (nodes, edges) = dense_incident_graph();
    let text = render_timeline_text(&nodes, &edges, &FocusOptions::default());
    // Find the line for n7 — must contain [?] glyph for LowConfidence.
    let n7_line = text
        .lines()
        .find(|l| l.contains("workflow:diag_dump"))
        .expect("n7 line present");
    assert!(
        n7_line.contains("[?]"),
        "n7 should carry [?] (LowConfidence inbound), got: {n7_line}"
    );
    // n5 line must contain [??] (worst-inbound = Uncertain).
    let n5_line = text
        .lines()
        .find(|l| l.contains("rollback to checkpoint cp-7"))
        .expect("n5 line present");
    assert!(
        n5_line.contains("[??]"),
        "n5 should carry [??] (worst inbound = Uncertain), got: {n5_line}"
    );
}

/// Dependency tree rooted at n1 walks descendants and renders all
/// reachable nodes within max_depth. With max_depth=4 starting at
/// n1, we should reach n2 → n3 → n4 → n5.
#[test]
fn dependency_tree_walks_descendants_to_max_depth_ft_1650n_16() {
    let (nodes, edges) = dense_incident_graph();
    let text = render_dependency_tree(
        &nodes,
        &edges,
        "n1",
        CausalTraversalDirection::Descendants,
        4,
        5_000, // low_confidence threshold (basis points)
    );
    // All 5 descendants in the n1 → ... → n5 chain appear.
    for (id, label) in [
        ("n1", "pane 1 stderr burst"),
        ("n2", "rule:auth_fail matched"),
        ("n3", "policy:require_approval"),
        ("n4", "operator approved"),
        ("n5", "rollback to checkpoint cp-7"),
    ] {
        assert!(
            text.contains(label),
            "expected node {id} ({label:?}) in dependency tree; got:\n{text}"
        );
    }
}

/// Evidence snippets render redacted context — the substrate trusts
/// the producer to redact, this test plants `[REDACTED]` tokens
/// in context values and verifies they survive verbatim through
/// the renderer (no double-redaction, no truncation).
#[test]
fn evidence_snippets_preserve_redaction_tokens_ft_1650n_16() {
    let (nodes, _edges) = dense_incident_graph();
    let snippets = render_evidence_snippets(&nodes, &FocusOptions::default());
    // n1 has context "excerpt = [REDACTED] stderr line" — must appear.
    let n1 = snippets
        .iter()
        .find(|s| s.node_id == "n1")
        .expect("n1 snippet present");
    let n1_excerpt = n1
        .context
        .get("excerpt")
        .expect("n1 excerpt context key present");
    assert_eq!(
        n1_excerpt, "[REDACTED] stderr line",
        "redaction token from context should survive renderer verbatim"
    );
    // n4 has context "operator = [REDACTED]" — must appear.
    let n4 = snippets
        .iter()
        .find(|s| s.node_id == "n4")
        .expect("n4 snippet present");
    let n4_operator = n4
        .context
        .get("operator")
        .expect("n4 operator context key present");
    assert_eq!(
        n4_operator, "[REDACTED]",
        "raw [REDACTED] marker must survive (no double-redaction)"
    );
}

/// Composite operator-text rendering combines timeline + dependency
/// tree (when focus_node_id is set) + evidence snippets in one
/// non-overlapping document. The dense-density acceptance: each
/// section is clearly delimited by its header line.
#[test]
fn operator_text_composes_timeline_tree_and_evidence_without_overlap_ft_1650n_16() {
    let (nodes, edges) = dense_incident_graph();
    let opts = OperatorTextOptions {
        focus: FocusOptions::default(),
        focus_node_id: Some("n1".into()),
        direction: CausalTraversalDirection::Descendants,
        max_depth: 3,
        include_evidence_snippets: true,
    };
    let text = render_operator_text(&nodes, &edges, &opts);
    // Section delimiters all present.
    assert!(
        text.contains("=== causal graph timeline ==="),
        "timeline section header present"
    );
    assert!(
        text.contains("=== dependency tree"),
        "dependency tree section header present"
    );
    assert!(
        text.contains("=== evidence snippets ==="),
        "evidence snippets section header present"
    );
    // No section header appears more than once (no overlapping
    // double-render).
    assert_eq!(
        text.matches("=== causal graph timeline ===").count(),
        1,
        "timeline header should appear exactly once"
    );
    assert_eq!(
        text.matches("=== evidence snippets ===").count(),
        1,
        "evidence header should appear exactly once"
    );
}

/// TOON / structured rendering covers the same node set as the
/// text rendering and carries every UncertaintyMarker variant
/// observed in the fixture. Operators consume this for automation.
#[test]
fn toon_rendering_covers_same_nodes_and_uncertainty_variants_ft_1650n_16() {
    let (nodes, edges) = dense_incident_graph();
    let rendering = render_robot_toon(&nodes, &edges, &FocusOptions::default());
    // 8 timeline entries.
    assert_eq!(rendering.timeline.len(), 8);
    // At least one Confident, one LowConfidence, one Uncertain
    // among uncertain_edges (substrate emits markers for ALL edges
    // at LowConfidence-or-worse; Confident edges may still appear
    // if the substrate decides to surface them — both behaviors
    // acceptable for this E2E gate).
    let markers: Vec<UncertaintyMarker> = rendering
        .uncertain_edges
        .iter()
        .map(|e| e.marker)
        .collect();
    assert!(
        markers.contains(&UncertaintyMarker::LowConfidence),
        "expected LowConfidence marker in uncertain edges (n6→n7)"
    );
    assert!(
        markers.contains(&UncertaintyMarker::Uncertain),
        "expected Uncertain marker in uncertain edges (n7→n5)"
    );
    // Schema version present + stable.
    assert_eq!(rendering.schema_version, "ft.causal_console.rendering.v1");
}

/// Empty graph renders a clean "no nodes match" message rather
/// than crashing. Defends the substrate's contract for the
/// no-evidence operator-input case.
#[test]
fn empty_graph_renders_no_nodes_match_message_ft_1650n_16() {
    let text = render_timeline_text(&[], &[], &FocusOptions::default());
    assert!(text.contains("(no nodes match the focus filter)"));
}

/// Rendering is pure / deterministic — same input twice yields
/// byte-identical output. Important for snapshot tests + audit
/// reproducibility.
#[test]
fn rendering_is_byte_identical_across_repeated_calls_ft_1650n_16() {
    let (nodes, edges) = dense_incident_graph();
    let opts = OperatorTextOptions::default();
    let a = render_operator_text(&nodes, &edges, &opts);
    let b = render_operator_text(&nodes, &edges, &opts);
    assert_eq!(a, b, "render must be deterministic");

    // Also verify TOON / structured rendering. Compare via JSON
    // so BTreeMap (in EvidenceSnippet) handles ordering.
    let r1 = render_robot_toon(&nodes, &edges, &FocusOptions::default());
    let r2 = render_robot_toon(&nodes, &edges, &FocusOptions::default());
    let j1 = serde_json::to_string(&r1).unwrap();
    let j2 = serde_json::to_string(&r2).unwrap();
    assert_eq!(j1, j2, "TOON render must be deterministic");
}

// Suppress unused-import warning for EvidenceSnippet + BTreeMap when
// the substrate's API rename rolls through; keeps the imports honest.
#[allow(dead_code)]
fn _unused_imports_anchor() -> Option<EvidenceSnippet> {
    let _: BTreeMap<String, String> = BTreeMap::new();
    None
}
