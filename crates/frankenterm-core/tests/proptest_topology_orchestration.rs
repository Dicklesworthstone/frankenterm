//! Property-based tests for topology_orchestration (ft-3681t.2.2).
//!
//! Coverage: LayoutNode tree invariants, serde roundtrips, rebalance properties,
//! validate_op with arbitrary states, audit log eviction, template registry,
//! focus group creation, TopologyPlan validation, and TopologyError Display.

use proptest::prelude::*;

use frankenterm_core::topology_orchestration::{
    FocusGroup, LayoutNode, LayoutTemplate, OpCheckResult, TemplateRegistry, TopologyAuditEntry,
    TopologyError, TopologyMoveDirection, TopologyOp, TopologyOrchestrator,
    TopologySplitDirection,
};
use frankenterm_core::session_topology::{
    LifecycleEntityKind, LifecycleIdentity, LifecycleRegistry, LifecycleState,
    MuxPaneLifecycleState, PaneNode,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn pane_id(id: u64) -> LifecycleIdentity {
    LifecycleIdentity::new(LifecycleEntityKind::Pane, "ws", "local", id, 1)
}

fn window_id(id: u64) -> LifecycleIdentity {
    LifecycleIdentity::new(LifecycleEntityKind::Window, "ws", "local", id, 1)
}

const F64_TOLERANCE: f64 = 1e-12;

fn f64_approx_eq(a: f64, b: f64) -> bool {
    (a - b).abs() < F64_TOLERANCE
}

fn layout_nodes_approx_eq(a: &LayoutNode, b: &LayoutNode) -> bool {
    match (a, b) {
        (
            LayoutNode::Slot { role: ra, weight: wa },
            LayoutNode::Slot { role: rb, weight: wb },
        ) => ra == rb && f64_approx_eq(*wa, *wb),
        (LayoutNode::HSplit { children: ca }, LayoutNode::HSplit { children: cb })
        | (LayoutNode::VSplit { children: ca }, LayoutNode::VSplit { children: cb }) => {
            ca.len() == cb.len()
                && ca
                    .iter()
                    .zip(cb.iter())
                    .all(|(a, b)| layout_nodes_approx_eq(a, b))
        }
        _ => false,
    }
}

fn topology_ops_approx_eq(a: &TopologyOp, b: &TopologyOp) -> bool {
    match (a, b) {
        (
            TopologyOp::Split {
                target: ta,
                direction: da,
                ratio: ra,
            },
            TopologyOp::Split {
                target: tb,
                direction: db,
                ratio: rb,
            },
        ) => ta == tb && da == db && f64_approx_eq(*ra, *rb),
        _ => a == b,
    }
}

fn make_registry(pane_ids: &[u64]) -> LifecycleRegistry {
    let mut reg = LifecycleRegistry::new();
    for &pid in pane_ids {
        reg.register_entity(
            pane_id(pid),
            LifecycleState::Pane(MuxPaneLifecycleState::Running),
            0,
        )
        .ok();
    }
    reg
}

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn arb_split_direction() -> impl Strategy<Value = TopologySplitDirection> {
    prop_oneof![
        Just(TopologySplitDirection::Left),
        Just(TopologySplitDirection::Right),
        Just(TopologySplitDirection::Top),
        Just(TopologySplitDirection::Bottom),
    ]
}

fn arb_move_direction() -> impl Strategy<Value = TopologyMoveDirection> {
    prop_oneof![
        Just(TopologyMoveDirection::Left),
        Just(TopologyMoveDirection::Right),
        Just(TopologyMoveDirection::Up),
        Just(TopologyMoveDirection::Down),
    ]
}

fn arb_pane_state() -> impl Strategy<Value = MuxPaneLifecycleState> {
    prop_oneof![
        Just(MuxPaneLifecycleState::Provisioning),
        Just(MuxPaneLifecycleState::Ready),
        Just(MuxPaneLifecycleState::Running),
        Just(MuxPaneLifecycleState::Draining),
        Just(MuxPaneLifecycleState::Orphaned),
        Just(MuxPaneLifecycleState::Closed),
    ]
}

/// Generate a LayoutNode tree of bounded depth.
fn arb_layout_node(max_depth: u32) -> impl Strategy<Value = LayoutNode> {
    if max_depth == 0 {
        // Leaf only
        (prop::option::of("[a-z]{3,8}"), 0.1f64..10.0)
            .prop_map(|(role, weight)| LayoutNode::Slot { role, weight })
            .boxed()
    } else {
        let leaf = (prop::option::of("[a-z]{3,8}"), 0.1f64..10.0)
            .prop_map(|(role, weight)| LayoutNode::Slot { role, weight });
        prop_oneof![
            3 => leaf,
            1 => prop::collection::vec(arb_layout_node(max_depth - 1), 2..=4)
                .prop_map(|children| LayoutNode::HSplit { children }),
            1 => prop::collection::vec(arb_layout_node(max_depth - 1), 2..=4)
                .prop_map(|children| LayoutNode::VSplit { children }),
        ]
        .boxed()
    }
}

fn arb_op_check_result() -> impl Strategy<Value = OpCheckResult> {
    prop_oneof![
        Just(OpCheckResult::Ok),
        "[a-z]{3,12}".prop_map(|id| OpCheckResult::NotFound { identity: id }),
        ("[a-z]{3,12}", "[a-z]{3,12}", "[a-z]{3,20}").prop_map(|(id, st, reason)| {
            OpCheckResult::InvalidState {
                identity: id,
                current_state: st,
                reason,
            }
        }),
        "[a-z]{3,20}".prop_map(|reason| OpCheckResult::ConstraintViolation { reason }),
    ]
}

fn arb_topology_error() -> impl Strategy<Value = TopologyError> {
    prop_oneof![
        "[a-z:0-9]{3,15}".prop_map(|id| TopologyError::EntityNotFound { identity: id }),
        ("[a-z:0-9]{3,15}", "[A-Za-z]{3,10}", "[a-z]{3,10}").prop_map(|(id, st, op)| {
            TopologyError::InvalidLifecycleState {
                identity: id,
                state: st,
                operation: op,
            }
        }),
        "[a-z-]{3,15}".prop_map(|name| TopologyError::TemplateNotFound { name }),
        ("[a-z-]{3,15}", 1u32..100, 0u32..100).prop_map(|(t, req, avail)| {
            TopologyError::TemplatePaneMismatch {
                template: t,
                required: req,
                available: avail,
            }
        }),
        "[a-z:0-9]{3,15}".prop_map(|w| TopologyError::LastPaneProtection { window: w }),
        (-10.0f64..10.0).prop_map(|r| TopologyError::InvalidRatio { ratio: r }),
        "[a-z-]{3,15}".prop_map(|name| TopologyError::DuplicateFocusGroup { name }),
    ]
}

// ---------------------------------------------------------------------------
// LayoutNode tree invariants
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// slot_count is always >= 1 for any layout tree.
    #[test]
    fn slot_count_is_positive(node in arb_layout_node(3)) {
        assert!(node.slot_count() >= 1);
    }

    /// child_ratios sum to approximately 1.0 for split nodes.
    #[test]
    fn child_ratios_sum_to_one(node in arb_layout_node(2)) {
        let ratios = node.child_ratios();
        if !ratios.is_empty() {
            let sum: f64 = ratios.iter().sum();
            assert!(
                (sum - 1.0).abs() < 1e-9,
                "ratios sum to {sum}, expected ~1.0"
            );
        }
    }

    /// child_ratios are all non-negative.
    #[test]
    fn child_ratios_non_negative(node in arb_layout_node(2)) {
        for ratio in node.child_ratios() {
            assert!(ratio >= 0.0, "negative ratio: {ratio}");
        }
    }

    /// roles() returns the same count as slot_count() when all slots have roles.
    #[test]
    fn roles_count_le_slot_count(node in arb_layout_node(3)) {
        assert!(node.roles().len() as u32 <= node.slot_count());
    }

    /// weight() is always positive for nodes with positive-weight leaves.
    #[test]
    fn weight_is_positive(node in arb_layout_node(3)) {
        assert!(node.weight() > 0.0, "weight should be positive: {}", node.weight());
    }
}

// ---------------------------------------------------------------------------
// Serde roundtrips
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// LayoutNode serde roundtrip preserves structure (with f64 tolerance).
    #[test]
    fn layout_node_serde_roundtrip(node in arb_layout_node(2)) {
        let json = serde_json::to_string(&node).unwrap();
        let decoded: LayoutNode = serde_json::from_str(&json).unwrap();
        assert!(layout_nodes_approx_eq(&node, &decoded), "roundtrip mismatch");
    }

    /// TopologySplitDirection serde roundtrip.
    #[test]
    fn split_direction_serde_roundtrip(dir in arb_split_direction()) {
        let json = serde_json::to_string(&dir).unwrap();
        let decoded: TopologySplitDirection = serde_json::from_str(&json).unwrap();
        assert_eq!(dir, decoded);
    }

    /// TopologyMoveDirection serde roundtrip.
    #[test]
    fn move_direction_serde_roundtrip(dir in arb_move_direction()) {
        let json = serde_json::to_string(&dir).unwrap();
        let decoded: TopologyMoveDirection = serde_json::from_str(&json).unwrap();
        assert_eq!(dir, decoded);
    }

    /// OpCheckResult serde roundtrip.
    #[test]
    fn op_check_result_serde_roundtrip(result in arb_op_check_result()) {
        let json = serde_json::to_string(&result).unwrap();
        let decoded: OpCheckResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result, decoded);
    }

    /// TopologyError serde roundtrip.
    #[test]
    fn topology_error_serde_roundtrip(err in arb_topology_error()) {
        let json = serde_json::to_string(&err).unwrap();
        let decoded: TopologyError = serde_json::from_str(&json).unwrap();
        assert_eq!(err, decoded);
    }

    /// FocusGroup serde roundtrip.
    #[test]
    fn focus_group_serde_roundtrip(
        name in "[a-z]{3,10}",
        n_members in 1u64..5,
        focused in any::<bool>(),
        created_at in 0u64..u64::MAX / 2,
    ) {
        let members: Vec<LifecycleIdentity> = (1..=n_members).map(pane_id).collect();
        let group = FocusGroup {
            name,
            members,
            focused,
            created_at,
        };
        let json = serde_json::to_string(&group).unwrap();
        let decoded: FocusGroup = serde_json::from_str(&json).unwrap();
        assert_eq!(group, decoded);
    }

    /// LayoutTemplate serde roundtrip (with f64 tolerance).
    #[test]
    fn layout_template_serde_roundtrip(
        name in "[a-z-]{3,15}",
        desc in prop::option::of("[a-z ]{5,20}"),
        root in arb_layout_node(2),
        min_panes in 1u32..10,
        max_panes in prop::option::of(2u32..20),
    ) {
        let template = LayoutTemplate {
            name,
            description: desc,
            root,
            min_panes,
            max_panes,
        };
        let json = serde_json::to_string(&template).unwrap();
        let decoded: LayoutTemplate = serde_json::from_str(&json).unwrap();
        assert_eq!(template.name, decoded.name);
        assert_eq!(template.description, decoded.description);
        assert_eq!(template.min_panes, decoded.min_panes);
        assert_eq!(template.max_panes, decoded.max_panes);
        assert!(layout_nodes_approx_eq(&template.root, &decoded.root));
    }

    /// TopologyOp::Split serde roundtrip (with f64 tolerance).
    #[test]
    fn topology_op_split_serde_roundtrip(
        pid in 1u64..1000,
        dir in arb_split_direction(),
        ratio in 0.01f64..0.99,
    ) {
        let op = TopologyOp::Split {
            target: pane_id(pid),
            direction: dir,
            ratio,
        };
        let json = serde_json::to_string(&op).unwrap();
        let decoded: TopologyOp = serde_json::from_str(&json).unwrap();
        assert!(topology_ops_approx_eq(&op, &decoded), "roundtrip mismatch for Split");
    }

    /// TopologyOp all variants serde roundtrip.
    #[test]
    fn topology_op_all_variants_serde(pid in 1u64..100) {
        let ops = vec![
            TopologyOp::Split {
                target: pane_id(pid),
                direction: TopologySplitDirection::Right,
                ratio: 0.5,
            },
            TopologyOp::Close { target: pane_id(pid) },
            TopologyOp::Swap { a: pane_id(pid), b: pane_id(pid + 1) },
            TopologyOp::Move {
                target: pane_id(pid),
                direction: TopologyMoveDirection::Up,
            },
            TopologyOp::ApplyTemplate {
                window: window_id(pid),
                template_name: "grid-2x2".into(),
            },
            TopologyOp::Rebalance { scope: window_id(pid) },
            TopologyOp::CreateFocusGroup {
                name: "group".into(),
                members: vec![pane_id(pid)],
            },
        ];
        for op in &ops {
            let json = serde_json::to_string(op).unwrap();
            let decoded: TopologyOp = serde_json::from_str(&json).unwrap();
            assert_eq!(op, &decoded);
        }
    }
}

// ---------------------------------------------------------------------------
// Rebalance invariants
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// Rebalance preserves leaf count.
    #[test]
    fn rebalance_preserves_leaf_count(
        node in arb_layout_node(3),
    ) {
        let slot_count = node.slot_count();
        // Convert to PaneNode, rebalance, count leaves
        let pane_ids: Vec<u64> = (1..=slot_count as u64).collect();
        let mut iter = pane_ids.iter().copied();
        if let Some(pane_node) = node.to_pane_node(&mut iter) {
            let rebalanced = TopologyOrchestrator::rebalance_tree(&pane_node);
            assert_eq!(
                count_leaves(&pane_node),
                count_leaves(&rebalanced),
                "rebalance changed leaf count"
            );
        }
    }

    /// Rebalance makes all sibling ratios equal.
    #[test]
    fn rebalance_equalizes_ratios(
        node in arb_layout_node(2),
    ) {
        let slot_count = node.slot_count();
        let pane_ids: Vec<u64> = (1..=slot_count as u64).collect();
        let mut iter = pane_ids.iter().copied();
        if let Some(pane_node) = node.to_pane_node(&mut iter) {
            let rebalanced = TopologyOrchestrator::rebalance_tree(&pane_node);
            assert_ratios_equal(&rebalanced);
        }
    }

    /// to_pane_node produces correct number of leaves.
    #[test]
    fn to_pane_node_leaf_count(node in arb_layout_node(3)) {
        let slot_count = node.slot_count();
        let pane_ids: Vec<u64> = (1..=slot_count as u64).collect();
        let mut iter = pane_ids.iter().copied();
        let pane_node = node.to_pane_node(&mut iter).unwrap();
        assert_eq!(count_leaves(&pane_node), slot_count as usize);
    }

    /// to_pane_node with insufficient IDs returns None.
    #[test]
    fn to_pane_node_insufficient_ids(node in arb_layout_node(2)) {
        let slot_count = node.slot_count();
        if slot_count > 1 {
            // Provide one fewer ID than needed
            let pane_ids: Vec<u64> = (1..slot_count as u64).collect();
            let mut iter = pane_ids.iter().copied();
            assert!(node.to_pane_node(&mut iter).is_none());
        }
    }
}

fn count_leaves(node: &PaneNode) -> usize {
    match node {
        PaneNode::Leaf { .. } => 1,
        PaneNode::HSplit { children } | PaneNode::VSplit { children } => {
            children.iter().map(|(_, child)| count_leaves(child)).sum()
        }
    }
}

fn assert_ratios_equal(node: &PaneNode) {
    match node {
        PaneNode::Leaf { .. } => {}
        PaneNode::HSplit { children } | PaneNode::VSplit { children } => {
            if children.len() > 1 {
                let expected = 1.0 / children.len() as f64;
                for (ratio, _) in children {
                    assert!(
                        (ratio - expected).abs() < 1e-9,
                        "ratio {ratio} != expected {expected}"
                    );
                }
            }
            for (_, child) in children {
                assert_ratios_equal(child);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Validation properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// Split with ratio in (0,1) on a Running pane always validates Ok.
    #[test]
    fn split_running_pane_valid(
        pid in 1u64..100,
        dir in arb_split_direction(),
        ratio in 0.01f64..0.99,
    ) {
        let orch = TopologyOrchestrator::new();
        let reg = make_registry(&[pid]);
        let op = TopologyOp::Split {
            target: pane_id(pid),
            direction: dir,
            ratio,
        };
        assert_eq!(orch.validate_op(&op, &reg), OpCheckResult::Ok);
    }

    /// Split with ratio outside (0,1) always fails with ConstraintViolation.
    #[test]
    fn split_invalid_ratio_fails(
        pid in 1u64..100,
        dir in arb_split_direction(),
        ratio in prop_oneof![
            (-10.0f64..=0.0),
            (1.0f64..=10.0),
        ],
    ) {
        let orch = TopologyOrchestrator::new();
        let reg = make_registry(&[pid]);
        let op = TopologyOp::Split {
            target: pane_id(pid),
            direction: dir,
            ratio,
        };
        assert!(
            matches!(orch.validate_op(&op, &reg), OpCheckResult::ConstraintViolation { .. }),
            "expected ConstraintViolation for ratio={ratio}"
        );
    }

    /// Split on nonexistent pane always fails with NotFound.
    #[test]
    fn split_missing_pane_not_found(pid in 100u64..200) {
        let orch = TopologyOrchestrator::new();
        let reg = make_registry(&[]); // empty registry
        let op = TopologyOp::Split {
            target: pane_id(pid),
            direction: TopologySplitDirection::Right,
            ratio: 0.5,
        };
        assert!(matches!(orch.validate_op(&op, &reg), OpCheckResult::NotFound { .. }));
    }

    /// Close on a Closed pane always fails with InvalidState.
    #[test]
    fn close_closed_pane_invalid(pid in 1u64..100) {
        let mut reg = LifecycleRegistry::new();
        reg.register_entity(
            pane_id(pid),
            LifecycleState::Pane(MuxPaneLifecycleState::Closed),
            0,
        ).ok();
        let orch = TopologyOrchestrator::new();
        let op = TopologyOp::Close { target: pane_id(pid) };
        assert!(matches!(orch.validate_op(&op, &reg), OpCheckResult::InvalidState { .. }));
    }

    /// Close on non-Closed pane states succeeds.
    #[test]
    fn close_non_closed_pane_valid(
        pid in 1u64..100,
        state in arb_pane_state().prop_filter("not Closed", |s| *s != MuxPaneLifecycleState::Closed),
    ) {
        let mut reg = LifecycleRegistry::new();
        reg.register_entity(pane_id(pid), LifecycleState::Pane(state), 0).ok();
        let orch = TopologyOrchestrator::new();
        let op = TopologyOp::Close { target: pane_id(pid) };
        assert_eq!(orch.validate_op(&op, &reg), OpCheckResult::Ok);
    }

    /// Split only accepts Running or Ready panes.
    #[test]
    fn split_only_running_or_ready(
        pid in 1u64..100,
        state in arb_pane_state(),
    ) {
        let mut reg = LifecycleRegistry::new();
        reg.register_entity(pane_id(pid), LifecycleState::Pane(state), 0).ok();
        let orch = TopologyOrchestrator::new();
        let op = TopologyOp::Split {
            target: pane_id(pid),
            direction: TopologySplitDirection::Right,
            ratio: 0.5,
        };
        let result = orch.validate_op(&op, &reg);
        match state {
            MuxPaneLifecycleState::Running | MuxPaneLifecycleState::Ready => {
                assert_eq!(result, OpCheckResult::Ok);
            }
            _ => {
                assert!(matches!(result, OpCheckResult::InvalidState { .. }));
            }
        }
    }

    /// Move only accepts Running or Ready panes.
    #[test]
    fn move_only_running_or_ready(
        pid in 1u64..100,
        state in arb_pane_state(),
        dir in arb_move_direction(),
    ) {
        let mut reg = LifecycleRegistry::new();
        reg.register_entity(pane_id(pid), LifecycleState::Pane(state), 0).ok();
        let orch = TopologyOrchestrator::new();
        let op = TopologyOp::Move {
            target: pane_id(pid),
            direction: dir,
        };
        let result = orch.validate_op(&op, &reg);
        match state {
            MuxPaneLifecycleState::Running | MuxPaneLifecycleState::Ready => {
                assert_eq!(result, OpCheckResult::Ok);
            }
            _ => {
                assert!(matches!(result, OpCheckResult::InvalidState { .. }));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Plan validation properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    /// Plan with all valid ops is marked validated=true.
    #[test]
    fn plan_all_valid_ops_validated(n_panes in 2u64..6) {
        let pane_ids: Vec<u64> = (1..=n_panes).collect();
        let reg = make_registry(&pane_ids);
        let orch = TopologyOrchestrator::new();

        let ops: Vec<TopologyOp> = pane_ids.iter().map(|&pid| {
            TopologyOp::Split {
                target: pane_id(pid),
                direction: TopologySplitDirection::Right,
                ratio: 0.5,
            }
        }).collect();

        let plan = orch.validate_plan(ops, &reg);
        assert!(plan.validated);
        assert_eq!(plan.operations.len(), n_panes as usize);
    }

    /// Plan with any invalid op is marked validated=false.
    #[test]
    fn plan_any_invalid_not_validated(n_valid in 1u64..4) {
        let pane_ids: Vec<u64> = (1..=n_valid).collect();
        let reg = make_registry(&pane_ids);
        let orch = TopologyOrchestrator::new();

        let mut ops: Vec<TopologyOp> = pane_ids.iter().map(|&pid| {
            TopologyOp::Split {
                target: pane_id(pid),
                direction: TopologySplitDirection::Right,
                ratio: 0.5,
            }
        }).collect();

        // Add one invalid op (nonexistent pane)
        ops.push(TopologyOp::Close { target: pane_id(999) });

        let plan = orch.validate_plan(ops, &reg);
        assert!(!plan.validated);
    }
}

// ---------------------------------------------------------------------------
// Audit log properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    /// Audit log grows monotonically up to the eviction threshold.
    #[test]
    fn audit_log_grows_monotonically(n_records in 1u32..50) {
        let mut orch = TopologyOrchestrator::new();

        let mut prev_len = 0;
        for i in 0..n_records {
            orch.record_audit(
                TopologyOp::Rebalance { scope: window_id(i as u64) },
                true,
                None,
                None,
            );
            let cur_len = orch.audit_log().len();
            assert!(cur_len >= prev_len, "audit log shrank from {prev_len} to {cur_len}");
            prev_len = cur_len;
        }
        assert_eq!(orch.audit_log().len(), n_records as usize);
    }

    /// Audit log records preserve correlation_id.
    #[test]
    fn audit_log_preserves_correlation_id(corr_id in "[a-z0-9-]{5,15}") {
        let mut orch = TopologyOrchestrator::new();
        orch.record_audit(
            TopologyOp::Close { target: pane_id(1) },
            false,
            Some("test error".into()),
            Some(corr_id.clone()),
        );
        let entry = &orch.audit_log()[0];
        assert_eq!(entry.correlation_id.as_deref(), Some(corr_id.as_str()));
        assert!(!entry.succeeded);
        assert_eq!(entry.error.as_deref(), Some("test error"));
    }
}

// ---------------------------------------------------------------------------
// Template registry properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    /// Registering n distinct templates yields len() == n.
    #[test]
    fn template_registry_len_matches_distinct(
        names in prop::collection::hash_set("[a-z]{3,10}", 1..10),
    ) {
        let mut reg = TemplateRegistry::new();
        for name in &names {
            reg.register(LayoutTemplate {
                name: name.clone(),
                description: None,
                root: LayoutNode::Slot { role: None, weight: 1.0 },
                min_panes: 1,
                max_panes: Some(1),
            });
        }
        assert_eq!(reg.len(), names.len());
    }

    /// Overwriting a template name doesn't increase len.
    #[test]
    fn template_registry_overwrite_stable(name in "[a-z]{3,10}") {
        let mut reg = TemplateRegistry::new();
        for i in 0..5 {
            reg.register(LayoutTemplate {
                name: name.clone(),
                description: Some(format!("v{i}")),
                root: LayoutNode::Slot { role: None, weight: 1.0 },
                min_panes: 1,
                max_panes: None,
            });
        }
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.get(&name).unwrap().description.as_deref(), Some("v4"));
    }

    /// names() returns sorted output.
    #[test]
    fn template_registry_names_sorted(
        names in prop::collection::hash_set("[a-z]{3,10}", 1..10),
    ) {
        let mut reg = TemplateRegistry::new();
        for name in &names {
            reg.register(LayoutTemplate {
                name: name.clone(),
                description: None,
                root: LayoutNode::Slot { role: None, weight: 1.0 },
                min_panes: 1,
                max_panes: None,
            });
        }
        let result = reg.names();
        let mut sorted = result.clone();
        sorted.sort_unstable();
        assert_eq!(result, sorted);
    }
}

// ---------------------------------------------------------------------------
// Focus group properties
// ---------------------------------------------------------------------------

#[test]
fn focus_group_toggle_alternates() {
    let mut orch = TopologyOrchestrator::new();
    let reg = make_registry(&[1, 2, 3]);
    orch.create_focus_group("g".into(), vec![pane_id(1), pane_id(2)], &reg)
        .unwrap();

    for i in 0..10 {
        let state = orch.toggle_focus_group("g").unwrap();
        assert_eq!(state, i % 2 == 0, "toggle #{i} should be {}", i % 2 == 0);
    }
}

#[test]
fn focus_group_remove_then_recreate() {
    let mut orch = TopologyOrchestrator::new();
    let reg = make_registry(&[1]);

    orch.create_focus_group("g".into(), vec![pane_id(1)], &reg)
        .unwrap();
    assert!(orch.remove_focus_group("g"));

    // Can recreate after removal
    let result = orch.create_focus_group("g".into(), vec![pane_id(1)], &reg);
    assert!(result.is_ok());
}

// ---------------------------------------------------------------------------
// TopologyError Display
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// All TopologyError variants produce non-empty Display output.
    #[test]
    fn topology_error_display_nonempty(err in arb_topology_error()) {
        let display = err.to_string();
        assert!(!display.is_empty());
    }
}

// ---------------------------------------------------------------------------
// Template layout generation properties
// ---------------------------------------------------------------------------

#[test]
fn layout_from_all_default_templates() {
    let orch = TopologyOrchestrator::new();

    // side-by-side: 2 panes
    let result = orch.layout_from_template("side-by-side", &[1, 2]);
    assert!(result.is_ok());
    assert_eq!(count_leaves(&result.unwrap()), 2);

    // primary-sidebar: 2 panes
    let result = orch.layout_from_template("primary-sidebar", &[1, 2]);
    assert!(result.is_ok());
    assert_eq!(count_leaves(&result.unwrap()), 2);

    // grid-2x2: 4 panes
    let result = orch.layout_from_template("grid-2x2", &[1, 2, 3, 4]);
    assert!(result.is_ok());
    assert_eq!(count_leaves(&result.unwrap()), 4);

    // swarm-1+3: 4 panes
    let result = orch.layout_from_template("swarm-1+3", &[1, 2, 3, 4]);
    assert!(result.is_ok());
    assert_eq!(count_leaves(&result.unwrap()), 4);
}

#[test]
fn layout_from_template_exact_pane_count() {
    let orch = TopologyOrchestrator::new();

    // Fewer panes than needed → error
    assert!(orch.layout_from_template("grid-2x2", &[1]).is_err());

    // More panes than max → error
    assert!(orch.layout_from_template("side-by-side", &[1, 2, 3]).is_err());

    // Exact match → ok
    assert!(orch.layout_from_template("side-by-side", &[1, 2]).is_ok());
}

// ---------------------------------------------------------------------------
// TopologySplitDirection ↔ wezterm conversion
// ---------------------------------------------------------------------------

#[test]
fn split_direction_wezterm_roundtrip_all() {
    use frankenterm_core::wezterm::SplitDirection;

    let mapping = [
        (TopologySplitDirection::Left, SplitDirection::Left),
        (TopologySplitDirection::Right, SplitDirection::Right),
        (TopologySplitDirection::Top, SplitDirection::Top),
        (TopologySplitDirection::Bottom, SplitDirection::Bottom),
    ];

    for (topo, wez) in &mapping {
        assert_eq!(topo.to_wezterm(), *wez);
    }
}

// ---------------------------------------------------------------------------
// TopologyAuditEntry serde
// ---------------------------------------------------------------------------

#[test]
fn audit_entry_serde_roundtrip() {
    let entry = TopologyAuditEntry {
        op: TopologyOp::Split {
            target: pane_id(42),
            direction: TopologySplitDirection::Bottom,
            ratio: 0.3,
        },
        succeeded: true,
        error: None,
        timestamp: 1234567890,
        correlation_id: Some("corr-xyz".into()),
    };

    let json = serde_json::to_string(&entry).unwrap();
    let decoded: TopologyAuditEntry = serde_json::from_str(&json).unwrap();
    assert_eq!(entry.succeeded, decoded.succeeded);
    assert_eq!(entry.timestamp, decoded.timestamp);
    assert_eq!(entry.correlation_id, decoded.correlation_id);
}
