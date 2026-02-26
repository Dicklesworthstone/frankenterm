//! Property tests for replay diff semantics and budget monotonicity.
//!
//! This suite covers P-16 through P-20 for `ft-og6q6.7.2`.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use frankenterm_core::differential_snapshot::BaseSnapshot;
use frankenterm_core::session_pane_state::{PaneStateSnapshot, ScrollbackRef, TerminalState};
use frankenterm_core::session_topology::{
    PaneNode, TOPOLOGY_SCHEMA_VERSION, TabSnapshot, TopologySnapshot, WindowSnapshot,
};
use proptest::prelude::ProptestConfig;
use proptest::prelude::*;

fn proptest_cases() -> u32 {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|raw| raw.parse::<u32>().ok())
        .map(|cases| cases.max(100))
        .unwrap_or(100)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum DivergenceKind {
    Added,
    Removed,
    Changed,
    TopologyChanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum DivergenceSeverity {
    Info,
    Warning,
    Error,
    Critical,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RootCause {
    node_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReplayDivergence {
    kind: DivergenceKind,
    node_id: String,
    severity: DivergenceSeverity,
    root_cause: Option<RootCause>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RegressionBudget {
    max_divergences: usize,
    max_critical: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BudgetOutcome {
    Pass,
    Fail,
}

fn make_terminal(rows: u16, cols: u16) -> TerminalState {
    TerminalState {
        rows,
        cols,
        cursor_row: 0,
        cursor_col: 0,
        is_alt_screen: false,
        title: "proptest-replay-diff".to_string(),
    }
}

fn pane_node_id(pane_id: u64) -> String {
    format!("pane:{pane_id}")
}

fn make_topology(pane_ids: &[u64], captured_at: u64) -> TopologySnapshot {
    let tabs: Vec<TabSnapshot> = pane_ids
        .iter()
        .copied()
        .map(|pane_id| TabSnapshot {
            tab_id: pane_id,
            title: Some(format!("tab-{pane_id}")),
            pane_tree: PaneNode::Leaf {
                pane_id,
                rows: 24,
                cols: 80,
                cwd: None,
                title: None,
                is_active: false,
            },
            active_pane_id: Some(pane_id),
        })
        .collect();

    TopologySnapshot {
        schema_version: TOPOLOGY_SCHEMA_VERSION,
        captured_at,
        workspace_id: None,
        windows: vec![WindowSnapshot {
            window_id: 1,
            title: Some("proptest-replay-diff".to_string()),
            position: None,
            size: None,
            tabs,
            active_tab_index: Some(0),
        }],
    }
}

fn snapshot_from_specs(
    specs: Vec<(u64, u16, u16, u64, u64)>,
    captured_at: u64,
    topology_salt: u8,
) -> BaseSnapshot {
    let mut dedup: BTreeMap<u64, PaneStateSnapshot> = BTreeMap::new();

    for (pane_id, rows, cols, tick, cwd_salt) in specs {
        let pane = PaneStateSnapshot::new(pane_id, tick, make_terminal(rows, cols))
            .with_cwd(format!("/tmp/pane-{pane_id}-{cwd_salt}-{topology_salt}"))
            .with_scrollback(ScrollbackRef {
                output_segments_seq: tick as i64,
                total_lines_captured: 100 + tick,
                last_capture_at: tick,
            });
        dedup.insert(pane_id, pane);
    }

    let pane_ids: Vec<u64> = dedup.keys().copied().collect();
    let topology = make_topology(
        &pane_ids,
        captured_at.saturating_add(u64::from(topology_salt)),
    );
    BaseSnapshot::new(captured_at, topology, dedup.into_values().collect())
}

fn arb_snapshot(max_panes: usize) -> impl Strategy<Value = BaseSnapshot> {
    (
        prop::collection::vec(
            (
                0u64..=10,
                20u16..=60,
                40u16..=200,
                1u64..=20_000,
                0u64..=500,
            ),
            0..=max_panes,
        ),
        1_000u64..=2_000_000u64,
        any::<u8>(),
    )
        .prop_map(|(specs, captured_at, topology_salt)| {
            snapshot_from_specs(specs, captured_at, topology_salt)
        })
}

fn arb_snapshot_pair() -> impl Strategy<Value = (BaseSnapshot, BaseSnapshot)> {
    (arb_snapshot(8), arb_snapshot(8))
}

fn diff_snapshots(left: &BaseSnapshot, right: &BaseSnapshot) -> Vec<ReplayDivergence> {
    let mut divergences = Vec::new();

    let mut pane_ids = BTreeSet::new();
    pane_ids.extend(left.pane_states.keys().copied());
    pane_ids.extend(right.pane_states.keys().copied());

    for pane_id in pane_ids {
        let node_id = pane_node_id(pane_id);
        match (
            left.pane_states.get(&pane_id),
            right.pane_states.get(&pane_id),
        ) {
            (None, Some(_)) => divergences.push(ReplayDivergence {
                kind: DivergenceKind::Added,
                node_id: node_id.clone(),
                severity: DivergenceSeverity::Critical,
                root_cause: Some(RootCause {
                    node_id: node_id.clone(),
                }),
            }),
            (Some(_), None) => divergences.push(ReplayDivergence {
                kind: DivergenceKind::Removed,
                node_id: node_id.clone(),
                severity: DivergenceSeverity::Critical,
                root_cause: Some(RootCause {
                    node_id: node_id.clone(),
                }),
            }),
            (Some(left_state), Some(right_state)) if left_state != right_state => {
                divergences.push(ReplayDivergence {
                    kind: DivergenceKind::Changed,
                    node_id: node_id.clone(),
                    severity: DivergenceSeverity::Error,
                    root_cause: Some(RootCause {
                        node_id: node_id.clone(),
                    }),
                });
            }
            _ => {}
        }
    }

    if left.topology != right.topology {
        divergences.push(ReplayDivergence {
            kind: DivergenceKind::TopologyChanged,
            node_id: "topology".to_string(),
            severity: DivergenceSeverity::Warning,
            root_cause: Some(RootCause {
                node_id: "topology".to_string(),
            }),
        });
    }

    divergences.sort_by(|a, b| {
        (a.kind, a.node_id.as_str(), a.severity).cmp(&(b.kind, b.node_id.as_str(), b.severity))
    });
    divergences
}

fn aggregate_severity(divergences: &[ReplayDivergence]) -> DivergenceSeverity {
    divergences
        .iter()
        .map(|divergence| divergence.severity)
        .max()
        .unwrap_or(DivergenceSeverity::Info)
}

fn evaluate_budget(divergences: &[ReplayDivergence], budget: RegressionBudget) -> BudgetOutcome {
    let total = divergences.len();
    let critical = divergences
        .iter()
        .filter(|divergence| divergence.severity == DivergenceSeverity::Critical)
        .count();

    if total <= budget.max_divergences && critical <= budget.max_critical {
        BudgetOutcome::Pass
    } else {
        BudgetOutcome::Fail
    }
}

fn divergence_nodes_for_kind(
    divergences: &[ReplayDivergence],
    kind: DivergenceKind,
) -> HashSet<String> {
    divergences
        .iter()
        .filter(|divergence| divergence.kind == kind)
        .map(|divergence| divergence.node_id.clone())
        .collect()
}

fn arb_budget_pair() -> impl Strategy<Value = (RegressionBudget, RegressionBudget)> {
    (0usize..=20, 0usize..=20, 0usize..=20, 0usize..=20).prop_map(
        |(strict_max_divergences, strict_max_critical, relax_div_delta, relax_critical_delta)| {
            let strict = RegressionBudget {
                max_divergences: strict_max_divergences,
                max_critical: strict_max_critical,
            };
            let relaxed = RegressionBudget {
                max_divergences: strict_max_divergences.saturating_add(relax_div_delta),
                max_critical: strict_max_critical.saturating_add(relax_critical_delta),
            };
            (strict, relaxed)
        },
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(proptest_cases()))]

    /// P-16: Self-equivalence — diff(A, A) returns no divergences.
    #[test]
    fn p16_self_equivalence(snapshot in arb_snapshot(8)) {
        let divergences = diff_snapshots(&snapshot, &snapshot);
        prop_assert!(divergences.is_empty());
    }

    /// P-17: Antisymmetry — added/removed swap between diff(A,B) and diff(B,A).
    #[test]
    fn p17_antisymmetry(snapshot_pair in arb_snapshot_pair()) {
        let (left, right) = snapshot_pair;
        let left_to_right = diff_snapshots(&left, &right);
        let right_to_left = diff_snapshots(&right, &left);

        let lr_added = divergence_nodes_for_kind(&left_to_right, DivergenceKind::Added);
        let lr_removed = divergence_nodes_for_kind(&left_to_right, DivergenceKind::Removed);
        let rl_added = divergence_nodes_for_kind(&right_to_left, DivergenceKind::Added);
        let rl_removed = divergence_nodes_for_kind(&right_to_left, DivergenceKind::Removed);

        prop_assert_eq!(lr_added, rl_removed);
        prop_assert_eq!(lr_removed, rl_added);

        let lr_changed = divergence_nodes_for_kind(&left_to_right, DivergenceKind::Changed);
        let rl_changed = divergence_nodes_for_kind(&right_to_left, DivergenceKind::Changed);
        prop_assert_eq!(lr_changed, rl_changed);

        let lr_topology = divergence_nodes_for_kind(&left_to_right, DivergenceKind::TopologyChanged);
        let rl_topology = divergence_nodes_for_kind(&right_to_left, DivergenceKind::TopologyChanged);
        prop_assert_eq!(lr_topology, rl_topology);
    }

    /// P-18: Root cause existence — each divergence root cause references a real node.
    #[test]
    fn p18_root_cause_references_real_node(snapshot_pair in arb_snapshot_pair()) {
        let (left, right) = snapshot_pair;
        let divergences = diff_snapshots(&left, &right);

        let mut known_nodes = HashSet::new();
        known_nodes.extend(left.pane_states.keys().copied().map(pane_node_id));
        known_nodes.extend(right.pane_states.keys().copied().map(pane_node_id));
        if left.topology != right.topology {
            known_nodes.insert("topology".to_string());
        }

        for divergence in &divergences {
            let root_cause = divergence.root_cause.as_ref();
            prop_assert!(root_cause.is_some());
            let root_cause = root_cause.expect("checked above");
            prop_assert!(known_nodes.contains(root_cause.node_id.as_str()));
            prop_assert_eq!(&root_cause.node_id, &divergence.node_id);
        }
    }

    /// P-19: Severity ordering — aggregate severity is at least the max individual severity.
    #[test]
    fn p19_severity_ordering(snapshot_pair in arb_snapshot_pair()) {
        let (left, right) = snapshot_pair;
        let divergences = diff_snapshots(&left, &right);
        let aggregate = aggregate_severity(&divergences);
        for divergence in &divergences {
            prop_assert!(aggregate >= divergence.severity);
        }
    }

    /// P-20: Budget monotonicity — relaxing thresholds never turns pass into fail.
    #[test]
    fn p20_budget_monotonicity(snapshot_pair in arb_snapshot_pair(), budgets in arb_budget_pair()) {
        let (left, right) = snapshot_pair;
        let (strict, relaxed) = budgets;
        let divergences = diff_snapshots(&left, &right);

        let strict_outcome = evaluate_budget(&divergences, strict);
        let relaxed_outcome = evaluate_budget(&divergences, relaxed);

        if strict_outcome == BudgetOutcome::Pass {
            prop_assert_eq!(relaxed_outcome, BudgetOutcome::Pass);
        }
        if relaxed_outcome == BudgetOutcome::Fail {
            prop_assert_eq!(strict_outcome, BudgetOutcome::Fail);
        }
    }

    /// P-21: Topology-only changes remain warning-level topology divergences.
    #[test]
    fn p21_topology_only_change_is_warning(snapshot in arb_snapshot(8), delta in 1u64..=10_000u64) {
        let mut topology_only = snapshot.clone();
        topology_only.topology.captured_at = topology_only.topology.captured_at.saturating_add(delta);

        let divergences = diff_snapshots(&snapshot, &topology_only);
        prop_assert!(!divergences.is_empty());
        prop_assert!(divergences.iter().all(|divergence| divergence.kind == DivergenceKind::TopologyChanged));
        prop_assert!(divergences.iter().all(|divergence| divergence.severity == DivergenceSeverity::Warning));
    }
}
