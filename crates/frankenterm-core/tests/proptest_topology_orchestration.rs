//! Property-based tests for topology_orchestration serde and plan helpers.

use frankenterm_core::session_topology::{LifecycleEntityKind, LifecycleIdentity};
use frankenterm_core::topology_orchestration::{
    FocusGroup, LayoutNode, LayoutTemplate, OpCheckResult, TopologyAuditEntry, TopologyError,
    TopologyMoveDirection, TopologyOp, TopologyPlan, TopologySplitDirection, ValidatedOp,
};
use frankenterm_core::wezterm::SplitDirection;
use proptest::prelude::*;

fn arb_entity_kind() -> impl Strategy<Value = LifecycleEntityKind> {
    prop_oneof![
        Just(LifecycleEntityKind::Session),
        Just(LifecycleEntityKind::Window),
        Just(LifecycleEntityKind::Pane),
        Just(LifecycleEntityKind::Agent),
    ]
}

fn arb_identity() -> impl Strategy<Value = LifecycleIdentity> {
    (
        arb_entity_kind(),
        "[a-z][a-z0-9_-]{0,15}",
        "[a-z][a-z0-9._-]{0,15}",
        any::<u64>(),
        any::<u64>(),
    )
        .prop_map(|(kind, workspace_id, domain, local_id, generation)| {
            LifecycleIdentity::new(kind, workspace_id, domain, local_id, generation)
        })
}

fn arb_layout_node() -> impl Strategy<Value = LayoutNode> {
    let leaf = (prop::option::of("[a-z][a-z0-9_-]{0,15}"), 0.1f64..10.0f64)
        .prop_map(|(role, weight)| LayoutNode::Slot { role, weight });

    leaf.prop_recursive(6, 64, 4, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 1..=4)
                .prop_map(|children| LayoutNode::HSplit { children }),
            prop::collection::vec(inner, 1..=4)
                .prop_map(|children| LayoutNode::VSplit { children }),
        ]
    })
}

fn count_slots(node: &LayoutNode) -> u32 {
    match node {
        LayoutNode::Slot { .. } => 1,
        LayoutNode::HSplit { children } | LayoutNode::VSplit { children } => {
            children.iter().map(count_slots).sum()
        }
    }
}

fn layout_nodes_close(left: &LayoutNode, right: &LayoutNode) -> bool {
    match (left, right) {
        (
            LayoutNode::Slot {
                role: left_role,
                weight: left_weight,
            },
            LayoutNode::Slot {
                role: right_role,
                weight: right_weight,
            },
        ) => left_role == right_role && (left_weight - right_weight).abs() < 1e-10,
        (LayoutNode::HSplit { children: left }, LayoutNode::HSplit { children: right })
        | (LayoutNode::VSplit { children: left }, LayoutNode::VSplit { children: right }) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right.iter())
                    .all(|(lhs, rhs)| layout_nodes_close(lhs, rhs))
        }
        _ => false,
    }
}

fn arb_layout_template() -> impl Strategy<Value = LayoutTemplate> {
    (
        "[a-z][a-z0-9_-]{2,20}",
        prop::option::of(".{0,40}"),
        arb_layout_node(),
    )
        .prop_map(|(name, description, root)| {
            let slots = count_slots(&root);
            LayoutTemplate {
                name,
                description,
                root,
                min_panes: slots.max(1),
                max_panes: Some(slots.max(1)),
            }
        })
}

fn arb_focus_group() -> impl Strategy<Value = FocusGroup> {
    (
        "[a-z][a-z0-9_-]{2,20}",
        prop::collection::vec(arb_identity(), 0..=6),
        any::<bool>(),
        any::<u64>(),
    )
        .prop_map(|(name, members, focused, created_at)| FocusGroup {
            name,
            members,
            focused,
            created_at,
        })
}

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

fn arb_topology_op() -> impl Strategy<Value = TopologyOp> {
    prop_oneof![
        (arb_identity(), arb_split_direction(), 0.0f64..1.0f64).prop_map(
            |(target, direction, ratio)| TopologyOp::Split {
                target,
                direction,
                ratio,
            }
        ),
        arb_identity().prop_map(|target| TopologyOp::Close { target }),
        (arb_identity(), arb_identity()).prop_map(|(a, b)| TopologyOp::Swap { a, b }),
        (arb_identity(), arb_move_direction())
            .prop_map(|(target, direction)| TopologyOp::Move { target, direction }),
        (arb_identity(), "[a-z][a-z0-9_-]{2,20}").prop_map(|(window, template_name)| {
            TopologyOp::ApplyTemplate {
                window,
                template_name,
            }
        }),
        arb_identity().prop_map(|scope| TopologyOp::Rebalance { scope }),
        (
            "[a-z][a-z0-9_-]{2,20}",
            prop::collection::vec(arb_identity(), 0..=6)
        )
            .prop_map(|(name, members)| { TopologyOp::CreateFocusGroup { name, members } }),
    ]
}

fn arb_check_result() -> impl Strategy<Value = OpCheckResult> {
    prop_oneof![
        Just(OpCheckResult::Ok),
        (".{1,30}", ".{1,20}", ".{1,40}",).prop_map(|(identity, current_state, reason)| {
            OpCheckResult::InvalidState {
                identity,
                current_state,
                reason,
            }
        }),
        ".{1,30}".prop_map(|identity| OpCheckResult::NotFound { identity }),
        ".{1,40}".prop_map(|reason| OpCheckResult::ConstraintViolation { reason }),
    ]
}

fn arb_validated_op() -> impl Strategy<Value = ValidatedOp> {
    (arb_topology_op(), arb_check_result()).prop_map(|(op, check)| ValidatedOp { op, check })
}

fn arb_topology_error() -> impl Strategy<Value = TopologyError> {
    prop_oneof![
        ".{1,30}".prop_map(|identity| TopologyError::EntityNotFound { identity }),
        (".{1,30}", ".{1,20}", ".{1,20}").prop_map(|(identity, state, operation)| {
            TopologyError::InvalidLifecycleState {
                identity,
                state,
                operation,
            }
        }),
        ".{1,20}".prop_map(|name| TopologyError::TemplateNotFound { name }),
        (".{1,20}", any::<u32>(), any::<u32>()).prop_map(|(template, required, available)| {
            TopologyError::TemplatePaneMismatch {
                template,
                required,
                available,
            }
        }),
        ".{1,20}".prop_map(|window| TopologyError::LastPaneProtection { window }),
        (0.0f64..2.0f64).prop_map(|ratio| TopologyError::InvalidRatio { ratio }),
        ".{1,20}".prop_map(|name| TopologyError::DuplicateFocusGroup { name }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn prop_layout_template_serde_roundtrip(template in arb_layout_template()) {
        let json = serde_json::to_string(&template).unwrap();
        let back: LayoutTemplate = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.name, template.name);
        prop_assert_eq!(back.description, template.description);
        prop_assert_eq!(back.min_panes, template.min_panes);
        prop_assert_eq!(back.max_panes, template.max_panes);
        prop_assert!(layout_nodes_close(&back.root, &template.root));
        prop_assert!(count_slots(&back.root) >= back.min_panes);
    }

    #[test]
    fn prop_focus_group_serde_roundtrip(group in arb_focus_group()) {
        let json = serde_json::to_string(&group).unwrap();
        let back: FocusGroup = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, group);
    }

    #[test]
    fn prop_topology_op_serde_roundtrip(op in arb_topology_op()) {
        let json = serde_json::to_string(&op).unwrap();
        let back: TopologyOp = serde_json::from_str(&json).unwrap();
        match (&back, &op) {
            (
                TopologyOp::Split {
                    target: back_target,
                    direction: back_direction,
                    ratio: back_ratio,
                },
                TopologyOp::Split {
                    target,
                    direction,
                    ratio,
                },
            ) => {
                prop_assert_eq!(back_target, target);
                prop_assert_eq!(back_direction, direction);
                prop_assert!((back_ratio - ratio).abs() < 1e-10);
            }
            _ => prop_assert_eq!(back, op),
        }
    }

    #[test]
    fn prop_topology_plan_serde_roundtrip(
        operations in prop::collection::vec(arb_validated_op(), 0..=8),
        created_at in any::<u64>(),
        validated in any::<bool>(),
    ) {
        let plan = TopologyPlan {
            operations,
            validated,
            created_at,
        };
        let json = serde_json::to_string(&plan).unwrap();
        let back: TopologyPlan = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.operations.len(), plan.operations.len());
        prop_assert_eq!(back.validated, plan.validated);
        prop_assert_eq!(back.created_at, plan.created_at);
    }

    #[test]
    fn prop_topology_audit_entry_serde_roundtrip(
        op in arb_topology_op(),
        succeeded in any::<bool>(),
        error in prop::option::of(".{1,40}"),
        timestamp in any::<u64>(),
        correlation_id in prop::option::of(".{1,20}"),
    ) {
        let entry = TopologyAuditEntry {
            op,
            succeeded,
            error,
            timestamp,
            correlation_id,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: TopologyAuditEntry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.succeeded, entry.succeeded);
        prop_assert_eq!(back.error, entry.error);
        prop_assert_eq!(back.timestamp, entry.timestamp);
        prop_assert_eq!(back.correlation_id, entry.correlation_id);
    }

    #[test]
    fn prop_topology_error_serde_roundtrip(err in arb_topology_error()) {
        let json = serde_json::to_string(&err).unwrap();
        let back: TopologyError = serde_json::from_str(&json).unwrap();
        match (&back, &err) {
            (
                TopologyError::InvalidRatio { ratio: back_ratio },
                TopologyError::InvalidRatio { ratio },
            ) => prop_assert!((back_ratio - ratio).abs() < 1e-10),
            _ => prop_assert_eq!(back, err),
        }
    }

    #[test]
    fn prop_split_direction_maps_to_wezterm(direction in arb_split_direction()) {
        let mapped = direction.to_wezterm();
        let expected = match direction {
            TopologySplitDirection::Left => SplitDirection::Left,
            TopologySplitDirection::Right => SplitDirection::Right,
            TopologySplitDirection::Top => SplitDirection::Top,
            TopologySplitDirection::Bottom => SplitDirection::Bottom,
        };
        prop_assert_eq!(mapped, expected);
    }
}
