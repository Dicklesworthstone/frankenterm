//! Render-thread call-graph audit substrate (ft-q6x91 /
//! ft-2okh0.3.2.cont).
//!
//! Pure-logic substrate covering the substrate-shaped slice
//! of ft-q6x91: the static-analysis audit schema that
//! confirms "no render-thread call path touches Mutation
//! guards" per the bead's acceptance criterion.
//!
//! Sibling already shipped: `render_snapshot_guard.rs`
//! (commit 25e095d10) — `SnapshotKind` ReadOnly/Mutation
//! marker + `GuardLifecycle` + `LockWaitDistribution`.
//!
//! ## What this module ships
//!
//! - `CallSiteId(u32)` opaque function-or-call-site
//!   identifier the integration's static analyzer assigns
//!   per source location (file_path + line + column hash).
//! - `GuardConstructionSite` — `{ id, snapshot_kind,
//!   file, line }` — every place the codebase constructs a
//!   guard.
//! - `RenderEntryPoint` — `{ id, function_name }` for known
//!   render entry points (`paint_impl`, `render_pane`,
//!   `screen_line::render`).
//! - `CallEdge { caller, callee }` — directed edge in the
//!   call graph.
//! - `CallGraph` — adjacency-list representation +
//!   `reachable_from` BFS for transitive-call analysis.
//! - `audit_render_call_graph` — the bead's acceptance
//!   predicate: every guard reachable from a `RenderEntryPoint`
//!   must have `SnapshotKind::ReadOnly`. Returns
//!   `AuditOutcome::{ Pass, FailedWithViolations }`.
//! - `AuditViolation` — `{ entry_point, guard_site, kind,
//!   path }` — the BFS path from entry to violation, so
//!   integration prints actionable error messages.
//! - `AuditConfig` — operator-tunable: error-as-warn for
//!   diagnostic builds, max-path-length cap.
//! - `RenderAuditTelemetry` per-session counters.
//!
//! ## What is deferred to ft-q6x91 follow-up
//!
//! - Custom dylint/clippy plugin or grep harness that
//!   populates `CallGraph` from the actual frankenterm-gui
//!   crate source.
//! - Wiring the audit into CI as a release gate.
//! - paint.rs rewrite to actually use TripleBuffer.read().

#![allow(dead_code)]

use crate::render_snapshot_guard::SnapshotKind;
use std::collections::{BTreeMap, BTreeSet};

// ============================================================================
// Identifiers
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct CallSiteId(pub u32);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GuardConstructionSite {
    pub id: CallSiteId,
    pub snapshot_kind: SnapshotKind,
    pub file: String,
    pub line: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RenderEntryPoint {
    pub id: CallSiteId,
    pub function_name: String,
}

// ============================================================================
// CallGraph
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CallEdge {
    pub caller: CallSiteId,
    pub callee: CallSiteId,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallGraph {
    /// Adjacency list: caller → set of callees.
    edges: BTreeMap<CallSiteId, BTreeSet<CallSiteId>>,
}

impl CallGraph {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_edge(&mut self, caller: CallSiteId, callee: CallSiteId) {
        self.edges.entry(caller).or_default().insert(callee);
    }

    /// Number of distinct callers in the graph.
    #[must_use]
    pub fn caller_count(&self) -> usize {
        self.edges.len()
    }

    /// BFS reachability from `root`. Returns the set of
    /// reachable nodes (including `root` itself).
    #[must_use]
    pub fn reachable_from(&self, root: CallSiteId) -> BTreeSet<CallSiteId> {
        let mut visited = BTreeSet::new();
        let mut frontier = vec![root];
        visited.insert(root);
        while let Some(node) = frontier.pop() {
            if let Some(callees) = self.edges.get(&node) {
                for callee in callees {
                    if visited.insert(*callee) {
                        frontier.push(*callee);
                    }
                }
            }
        }
        visited
    }

    /// BFS shortest-path from `root` to `target`. Returns the
    /// path (`root → ... → target`) or `None` when unreachable.
    /// Substrate's audit prints this path so the violation
    /// is actionable.
    #[must_use]
    pub fn shortest_path(
        &self,
        root: CallSiteId,
        target: CallSiteId,
        max_path_length: usize,
    ) -> Option<Vec<CallSiteId>> {
        if root == target {
            return Some(vec![root]);
        }
        let mut visited: BTreeMap<CallSiteId, CallSiteId> = BTreeMap::new();
        let mut frontier: std::collections::VecDeque<CallSiteId> =
            std::collections::VecDeque::new();
        frontier.push_back(root);
        visited.insert(root, root); // sentinel: parent of root is self
        while let Some(node) = frontier.pop_front() {
            if let Some(callees) = self.edges.get(&node) {
                for callee in callees {
                    if visited.contains_key(callee) {
                        continue;
                    }
                    visited.insert(*callee, node);
                    if *callee == target {
                        let mut path = vec![*callee];
                        let mut cursor = node;
                        while cursor != root {
                            path.push(cursor);
                            cursor = *visited.get(&cursor).expect("parent exists");
                            if path.len() > max_path_length {
                                return None;
                            }
                        }
                        path.push(root);
                        path.reverse();
                        return Some(path);
                    }
                    frontier.push_back(*callee);
                }
            }
        }
        None
    }
}

// ============================================================================
// Audit
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuditConfig {
    /// When true, audit treats violations as errors (release
    /// gate). When false, audit reports them as warnings
    /// (diagnostic builds).
    pub error_as_warn: bool,
    /// BFS path-length cap for violation tracebacks.
    pub max_path_length: usize,
}

pub const DEFAULT_MAX_PATH_LENGTH: usize = 32;

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            error_as_warn: false,
            max_path_length: DEFAULT_MAX_PATH_LENGTH,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditViolation {
    pub entry_point: CallSiteId,
    pub guard_site: CallSiteId,
    pub kind: SnapshotKind,
    pub path: Vec<CallSiteId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditOutcome {
    /// Every guard reachable from every render entry point
    /// is `ReadOnly`. Bead's release gate passes.
    Pass,
    /// One or more violations — render-thread reaches at
    /// least one `Mutation` guard. Bead's release gate
    /// blocks (unless `error_as_warn`).
    FailedWithViolations { violations: Vec<AuditViolation> },
}

impl AuditOutcome {
    #[must_use]
    pub fn is_pass(&self) -> bool {
        matches!(self, Self::Pass)
    }

    /// Whether the integration's CI gate should fail on this
    /// outcome. Returns `true` when violations exist AND the
    /// config does NOT have `error_as_warn` set. When
    /// `error_as_warn` is true (diagnostic builds), violations
    /// are reported but don't block release. Pass always
    /// returns `false` (no block).
    #[must_use]
    pub fn is_release_blocker(&self, config: AuditConfig) -> bool {
        match self {
            Self::Pass => false,
            Self::FailedWithViolations { .. } => !config.error_as_warn,
        }
    }

    #[must_use]
    pub fn violation_count(&self) -> usize {
        match self {
            Self::Pass => 0,
            Self::FailedWithViolations { violations } => violations.len(),
        }
    }
}

/// Bead's audit predicate. For each `RenderEntryPoint`,
/// BFS the call graph, intersect with `GuardConstructionSite`
/// list, and report any reachable guard whose
/// `snapshot_kind == Mutation`.
#[must_use]
pub fn audit_render_call_graph(
    graph: &CallGraph,
    render_entry_points: &[RenderEntryPoint],
    guard_sites: &[GuardConstructionSite],
    config: AuditConfig,
) -> AuditOutcome {
    let mut violations = Vec::new();
    for entry in render_entry_points {
        let reachable = graph.reachable_from(entry.id);
        for guard in guard_sites {
            if !reachable.contains(&guard.id) {
                continue;
            }
            if matches!(guard.snapshot_kind, SnapshotKind::Mutation) {
                let path = graph
                    .shortest_path(entry.id, guard.id, config.max_path_length)
                    .unwrap_or_else(|| vec![entry.id, guard.id]);
                violations.push(AuditViolation {
                    entry_point: entry.id,
                    guard_site: guard.id,
                    kind: guard.snapshot_kind,
                    path,
                });
            }
        }
    }
    if violations.is_empty() {
        AuditOutcome::Pass
    } else {
        AuditOutcome::FailedWithViolations { violations }
    }
}

// ============================================================================
// Telemetry
// ============================================================================

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RenderAuditTelemetry {
    pub audits_run_total: u64,
    pub audits_passed: u64,
    pub audits_failed: u64,
    pub violations_observed_total: u64,
    pub max_path_length_observed: u32,
}

impl RenderAuditTelemetry {
    pub fn record_outcome(&mut self, outcome: &AuditOutcome) {
        self.audits_run_total = self.audits_run_total.saturating_add(1);
        match outcome {
            AuditOutcome::Pass => {
                self.audits_passed = self.audits_passed.saturating_add(1);
            }
            AuditOutcome::FailedWithViolations { violations } => {
                self.audits_failed = self.audits_failed.saturating_add(1);
                self.violations_observed_total = self
                    .violations_observed_total
                    .saturating_add(violations.len() as u64);
                for v in violations {
                    let len = v.path.len() as u32;
                    if len > self.max_path_length_observed {
                        self.max_path_length_observed = len;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site(id: u32, kind: SnapshotKind) -> GuardConstructionSite {
        GuardConstructionSite {
            id: CallSiteId(id),
            snapshot_kind: kind,
            file: format!("file_{id}.rs"),
            line: id * 10,
        }
    }

    fn entry(id: u32, name: &str) -> RenderEntryPoint {
        RenderEntryPoint {
            id: CallSiteId(id),
            function_name: name.to_string(),
        }
    }

    // ----------------------------------------------------------------
    // CallGraph
    // ----------------------------------------------------------------

    #[test]
    fn graph_empty_no_edges() {
        let g = CallGraph::new();
        assert_eq!(g.caller_count(), 0);
        assert_eq!(g.reachable_from(CallSiteId(0)), {
            let mut s = BTreeSet::new();
            s.insert(CallSiteId(0));
            s
        });
    }

    #[test]
    fn graph_reachable_from_simple_chain() {
        let mut g = CallGraph::new();
        g.add_edge(CallSiteId(1), CallSiteId(2));
        g.add_edge(CallSiteId(2), CallSiteId(3));
        g.add_edge(CallSiteId(3), CallSiteId(4));
        let r = g.reachable_from(CallSiteId(1));
        assert!(r.contains(&CallSiteId(1)));
        assert!(r.contains(&CallSiteId(2)));
        assert!(r.contains(&CallSiteId(3)));
        assert!(r.contains(&CallSiteId(4)));
    }

    #[test]
    fn graph_reachable_from_handles_cycle() {
        let mut g = CallGraph::new();
        g.add_edge(CallSiteId(1), CallSiteId(2));
        g.add_edge(CallSiteId(2), CallSiteId(3));
        g.add_edge(CallSiteId(3), CallSiteId(1)); // cycle
        let r = g.reachable_from(CallSiteId(1));
        assert_eq!(r.len(), 3);
    }

    #[test]
    fn graph_reachable_from_disjoint_isolated() {
        let mut g = CallGraph::new();
        g.add_edge(CallSiteId(1), CallSiteId(2));
        g.add_edge(CallSiteId(10), CallSiteId(20));
        let r = g.reachable_from(CallSiteId(1));
        assert!(r.contains(&CallSiteId(2)));
        assert!(!r.contains(&CallSiteId(10)));
        assert!(!r.contains(&CallSiteId(20)));
    }

    #[test]
    fn graph_shortest_path_finds_short_route() {
        let mut g = CallGraph::new();
        // 1 → 2 → 3 → 4 (long route)
        g.add_edge(CallSiteId(1), CallSiteId(2));
        g.add_edge(CallSiteId(2), CallSiteId(3));
        g.add_edge(CallSiteId(3), CallSiteId(4));
        // 1 → 4 (short route)
        g.add_edge(CallSiteId(1), CallSiteId(4));
        let path = g.shortest_path(CallSiteId(1), CallSiteId(4), 32).unwrap();
        // BFS finds the 2-step path.
        assert_eq!(path.len(), 2);
        assert_eq!(path[0], CallSiteId(1));
        assert_eq!(path[1], CallSiteId(4));
    }

    #[test]
    fn graph_shortest_path_unreachable_returns_none() {
        let mut g = CallGraph::new();
        g.add_edge(CallSiteId(1), CallSiteId(2));
        let path = g.shortest_path(CallSiteId(1), CallSiteId(99), 32);
        assert!(path.is_none());
    }

    #[test]
    fn graph_shortest_path_self_loop() {
        let g = CallGraph::new();
        let path = g.shortest_path(CallSiteId(5), CallSiteId(5), 32).unwrap();
        assert_eq!(path, vec![CallSiteId(5)]);
    }

    // ----------------------------------------------------------------
    // AuditOutcome
    // ----------------------------------------------------------------

    #[test]
    fn outcome_pass_is_pass() {
        let o = AuditOutcome::Pass;
        assert!(o.is_pass());
        assert_eq!(o.violation_count(), 0);
    }

    #[test]
    fn outcome_violations_count() {
        let o = AuditOutcome::FailedWithViolations {
            violations: vec![
                AuditViolation {
                    entry_point: CallSiteId(1),
                    guard_site: CallSiteId(99),
                    kind: SnapshotKind::Mutation,
                    path: vec![],
                },
                AuditViolation {
                    entry_point: CallSiteId(2),
                    guard_site: CallSiteId(98),
                    kind: SnapshotKind::Mutation,
                    path: vec![],
                },
            ],
        };
        assert!(!o.is_pass());
        assert_eq!(o.violation_count(), 2);
    }

    #[test]
    fn outcome_is_release_blocker_pass_never_blocks() {
        let o = AuditOutcome::Pass;
        assert!(!o.is_release_blocker(AuditConfig::default()));
        assert!(!o.is_release_blocker(AuditConfig {
            error_as_warn: true,
            max_path_length: 32,
        }));
    }

    #[test]
    fn outcome_is_release_blocker_failed_blocks_in_default() {
        // Bug fix (ft-vhvba): error_as_warn=false (default) means
        // violations DO block.
        let o = AuditOutcome::FailedWithViolations {
            violations: vec![AuditViolation {
                entry_point: CallSiteId(1),
                guard_site: CallSiteId(99),
                kind: SnapshotKind::Mutation,
                path: vec![],
            }],
        };
        assert!(o.is_release_blocker(AuditConfig::default()));
    }

    #[test]
    fn outcome_is_release_blocker_failed_no_block_with_error_as_warn() {
        // Bug fix (ft-vhvba): error_as_warn=true (diagnostic
        // builds) means violations are reported but DON'T block.
        let o = AuditOutcome::FailedWithViolations {
            violations: vec![AuditViolation {
                entry_point: CallSiteId(1),
                guard_site: CallSiteId(99),
                kind: SnapshotKind::Mutation,
                path: vec![],
            }],
        };
        let cfg = AuditConfig {
            error_as_warn: true,
            max_path_length: 32,
        };
        assert!(!o.is_release_blocker(cfg));
        // violation_count still reports 1.
        assert_eq!(o.violation_count(), 1);
    }

    // ----------------------------------------------------------------
    // audit_render_call_graph
    // ----------------------------------------------------------------

    #[test]
    fn audit_pass_when_only_readonly_reachable() {
        // paint_impl (1) → render_pane (2) → acquire (3=ReadOnly)
        let mut g = CallGraph::new();
        g.add_edge(CallSiteId(1), CallSiteId(2));
        g.add_edge(CallSiteId(2), CallSiteId(3));
        let entries = [entry(1, "paint_impl")];
        let guards = [site(3, SnapshotKind::ReadOnly)];
        let outcome = audit_render_call_graph(&g, &entries, &guards, AuditConfig::default());
        assert!(outcome.is_pass());
    }

    #[test]
    fn audit_fail_when_mutation_reachable_from_render() {
        // paint_impl (1) → bad_helper (2) → mutation_guard (3)
        let mut g = CallGraph::new();
        g.add_edge(CallSiteId(1), CallSiteId(2));
        g.add_edge(CallSiteId(2), CallSiteId(3));
        let entries = [entry(1, "paint_impl")];
        let guards = [site(3, SnapshotKind::Mutation)];
        let outcome = audit_render_call_graph(&g, &entries, &guards, AuditConfig::default());
        assert!(!outcome.is_pass());
        assert_eq!(outcome.violation_count(), 1);
        if let AuditOutcome::FailedWithViolations { violations } = outcome {
            assert_eq!(violations[0].guard_site, CallSiteId(3));
            assert_eq!(violations[0].kind, SnapshotKind::Mutation);
            // Path: 1 → 2 → 3
            assert_eq!(
                violations[0].path,
                vec![CallSiteId(1), CallSiteId(2), CallSiteId(3),]
            );
        }
    }

    #[test]
    fn audit_pass_when_mutation_only_in_writer_path() {
        // input_thread (10) → mutation_guard (99); paint_impl
        // (1) doesn't reach 99. Bead-correct setup.
        let mut g = CallGraph::new();
        g.add_edge(CallSiteId(1), CallSiteId(2)); // render side
        g.add_edge(CallSiteId(10), CallSiteId(99)); // writer side
        let entries = [entry(1, "paint_impl")];
        let guards = [
            site(2, SnapshotKind::ReadOnly),
            site(99, SnapshotKind::Mutation),
        ];
        let outcome = audit_render_call_graph(&g, &entries, &guards, AuditConfig::default());
        assert!(outcome.is_pass());
    }

    #[test]
    fn audit_finds_violations_across_multiple_entries() {
        let mut g = CallGraph::new();
        g.add_edge(CallSiteId(1), CallSiteId(99));
        g.add_edge(CallSiteId(2), CallSiteId(99));
        let entries = [entry(1, "paint_impl"), entry(2, "render_pane")];
        let guards = [site(99, SnapshotKind::Mutation)];
        let outcome = audit_render_call_graph(&g, &entries, &guards, AuditConfig::default());
        // Two violations: one per entry-point reaching the
        // same guard.
        assert_eq!(outcome.violation_count(), 2);
    }

    #[test]
    fn audit_empty_inputs_pass() {
        let g = CallGraph::new();
        let outcome = audit_render_call_graph(&g, &[], &[], AuditConfig::default());
        assert!(outcome.is_pass());
    }

    #[test]
    fn audit_no_render_entry_points_pass() {
        // Mutation guards exist but no render entry points
        // declared — vacuously safe.
        let mut g = CallGraph::new();
        g.add_edge(CallSiteId(1), CallSiteId(2));
        let guards = [site(2, SnapshotKind::Mutation)];
        let outcome = audit_render_call_graph(&g, &[], &guards, AuditConfig::default());
        assert!(outcome.is_pass());
    }

    #[test]
    fn audit_no_guards_declared_pass() {
        let mut g = CallGraph::new();
        g.add_edge(CallSiteId(1), CallSiteId(2));
        let entries = [entry(1, "paint_impl")];
        let outcome = audit_render_call_graph(&g, &entries, &[], AuditConfig::default());
        assert!(outcome.is_pass());
    }

    #[test]
    fn audit_path_capped_at_max_length() {
        // Long chain — 10 hops to the violation.
        let mut g = CallGraph::new();
        for i in 1..10 {
            g.add_edge(CallSiteId(i), CallSiteId(i + 1));
        }
        let entries = [entry(1, "paint_impl")];
        let guards = [site(10, SnapshotKind::Mutation)];
        let cfg = AuditConfig {
            error_as_warn: false,
            max_path_length: 32, // ample
        };
        let outcome = audit_render_call_graph(&g, &entries, &guards, cfg);
        if let AuditOutcome::FailedWithViolations { violations } = outcome {
            assert_eq!(violations[0].path.len(), 10);
        } else {
            panic!("expected violations");
        }
    }

    // ----------------------------------------------------------------
    // RenderAuditTelemetry
    // ----------------------------------------------------------------

    #[test]
    fn telemetry_default_zero() {
        let t = RenderAuditTelemetry::default();
        assert_eq!(t.audits_run_total, 0);
    }

    #[test]
    fn telemetry_record_pass() {
        let mut t = RenderAuditTelemetry::default();
        t.record_outcome(&AuditOutcome::Pass);
        assert_eq!(t.audits_run_total, 1);
        assert_eq!(t.audits_passed, 1);
        assert_eq!(t.audits_failed, 0);
    }

    #[test]
    fn telemetry_record_failure_tracks_violation_count() {
        let mut t = RenderAuditTelemetry::default();
        let outcome = AuditOutcome::FailedWithViolations {
            violations: vec![AuditViolation {
                entry_point: CallSiteId(1),
                guard_site: CallSiteId(2),
                kind: SnapshotKind::Mutation,
                path: vec![CallSiteId(1), CallSiteId(2)],
            }],
        };
        t.record_outcome(&outcome);
        assert_eq!(t.audits_failed, 1);
        assert_eq!(t.violations_observed_total, 1);
        assert_eq!(t.max_path_length_observed, 2);
    }

    #[test]
    fn telemetry_max_path_length_takes_max() {
        let mut t = RenderAuditTelemetry::default();
        let short = AuditOutcome::FailedWithViolations {
            violations: vec![AuditViolation {
                entry_point: CallSiteId(1),
                guard_site: CallSiteId(2),
                kind: SnapshotKind::Mutation,
                path: vec![CallSiteId(1), CallSiteId(2)],
            }],
        };
        let long = AuditOutcome::FailedWithViolations {
            violations: vec![AuditViolation {
                entry_point: CallSiteId(1),
                guard_site: CallSiteId(5),
                kind: SnapshotKind::Mutation,
                path: vec![
                    CallSiteId(1),
                    CallSiteId(2),
                    CallSiteId(3),
                    CallSiteId(4),
                    CallSiteId(5),
                ],
            }],
        };
        t.record_outcome(&short);
        t.record_outcome(&long);
        // Longer path wins.
        assert_eq!(t.max_path_length_observed, 5);
    }

    // ----------------------------------------------------------------
    // Cross-cut scenarios
    // ----------------------------------------------------------------

    #[test]
    fn scenario_clean_render_path_passes() {
        // Realistic clean migration: paint_impl → render_pane
        // → screen_line → triple_buffer.read (ReadOnly).
        let mut g = CallGraph::new();
        g.add_edge(CallSiteId(1), CallSiteId(2));
        g.add_edge(CallSiteId(2), CallSiteId(3));
        g.add_edge(CallSiteId(3), CallSiteId(4));
        let entries = [entry(1, "paint_impl")];
        let guards = [site(4, SnapshotKind::ReadOnly)];
        let outcome = audit_render_call_graph(&g, &entries, &guards, AuditConfig::default());
        assert!(outcome.is_pass());
    }

    #[test]
    fn scenario_buggy_refactor_caught() {
        // A refactor accidentally adds a Mutation guard via
        // a shared helper. Substrate's audit catches it with
        // the precise call path.
        let mut g = CallGraph::new();
        g.add_edge(CallSiteId(1), CallSiteId(7)); // paint_impl → shared helper
        g.add_edge(CallSiteId(7), CallSiteId(99)); // helper → mutation guard
        // Writer side legitimately uses the same helper.
        g.add_edge(CallSiteId(20), CallSiteId(7));
        let entries = [entry(1, "paint_impl")];
        let guards = [site(99, SnapshotKind::Mutation)];
        let outcome = audit_render_call_graph(&g, &entries, &guards, AuditConfig::default());
        assert!(!outcome.is_pass());
        if let AuditOutcome::FailedWithViolations { violations } = outcome {
            assert_eq!(violations.len(), 1);
            // Path should be: 1 → 7 → 99
            assert_eq!(
                violations[0].path,
                vec![CallSiteId(1), CallSiteId(7), CallSiteId(99),]
            );
        }
    }

    #[test]
    fn scenario_multiple_entry_points_same_violation() {
        // paint_impl + render_pane both reach the same bad
        // mutation guard via different paths.
        let mut g = CallGraph::new();
        g.add_edge(CallSiteId(1), CallSiteId(99));
        g.add_edge(CallSiteId(2), CallSiteId(50));
        g.add_edge(CallSiteId(50), CallSiteId(99));
        let entries = [entry(1, "paint_impl"), entry(2, "render_pane")];
        let guards = [site(99, SnapshotKind::Mutation)];
        let outcome = audit_render_call_graph(&g, &entries, &guards, AuditConfig::default());
        assert_eq!(outcome.violation_count(), 2);
    }

    #[test]
    fn scenario_release_gate_passes_at_end_of_migration() {
        // Bead's acceptance bar: every render-thread guard
        // is ReadOnly. Substrate's audit runs in CI; passes.
        let mut g = CallGraph::new();
        for i in 1..5 {
            g.add_edge(CallSiteId(i), CallSiteId(i + 1));
        }
        let entries = [entry(1, "paint_impl")];
        let guards = [
            site(2, SnapshotKind::ReadOnly),
            site(3, SnapshotKind::ReadOnly),
            site(4, SnapshotKind::ReadOnly),
            site(5, SnapshotKind::ReadOnly),
        ];
        let outcome = audit_render_call_graph(&g, &entries, &guards, AuditConfig::default());
        assert!(outcome.is_pass());
    }

    #[test]
    fn scenario_writer_thread_mutations_dont_trigger_violations() {
        // Bead's "DO NOT BREAK": writer side legitimately
        // uses Mutation guards. Audit only inspects render
        // entry points, not all entry points.
        let mut g = CallGraph::new();
        // Render side
        g.add_edge(CallSiteId(1), CallSiteId(2));
        // Writer side (input thread)
        g.add_edge(CallSiteId(100), CallSiteId(101));
        g.add_edge(CallSiteId(101), CallSiteId(200));
        let entries = [entry(1, "paint_impl")];
        let guards = [
            site(2, SnapshotKind::ReadOnly),
            site(200, SnapshotKind::Mutation), // legitimate writer guard
        ];
        let outcome = audit_render_call_graph(&g, &entries, &guards, AuditConfig::default());
        assert!(outcome.is_pass());
    }
}
