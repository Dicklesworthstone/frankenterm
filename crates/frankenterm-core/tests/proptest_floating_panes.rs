use std::collections::BTreeSet;

use proptest::prelude::*;

use frankenterm_core::floating_panes::{
    DragResizeState, FloatingPaneA11yKind, FloatingRect, FloatingZStack, PaneId, PanePosition,
    ResizeHandle, SnapEdge, ZOrder, apply_snap, make_a11y_message, snap_target,
};

fn arb_rect() -> impl Strategy<Value = FloatingRect> {
    (0u16..=512, 0u16..=512, 1u16..=128, 1u16..=128)
        .prop_map(|(x, y, width, height)| FloatingRect::new(x, y, width, height))
}

fn arb_resize_handle() -> impl Strategy<Value = ResizeHandle> {
    prop_oneof![
        Just(ResizeHandle::TopLeft),
        Just(ResizeHandle::Top),
        Just(ResizeHandle::TopRight),
        Just(ResizeHandle::Right),
        Just(ResizeHandle::BottomRight),
        Just(ResizeHandle::Bottom),
        Just(ResizeHandle::BottomLeft),
        Just(ResizeHandle::Left),
    ]
}

fn arb_snap_edge() -> impl Strategy<Value = SnapEdge> {
    prop_oneof![
        Just(SnapEdge::Top),
        Just(SnapEdge::Bottom),
        Just(SnapEdge::Left),
        Just(SnapEdge::Right),
        Just(SnapEdge::TopLeft),
        Just(SnapEdge::TopRight),
        Just(SnapEdge::BottomLeft),
        Just(SnapEdge::BottomRight),
    ]
}

fn order(stack: &FloatingZStack) -> Vec<PaneId> {
    stack.iter_back_to_front().map(|(id, _)| id).collect()
}

fn z_orders_are_strictly_ascending(stack: &FloatingZStack) -> bool {
    let mut previous = None;
    for (_, z) in stack.iter_back_to_front() {
        if previous.is_some_and(|prev| prev >= z) {
            return false;
        }
        previous = Some(z);
    }
    true
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    #[test]
    fn proptest_floating_panes_rect_and_position_helpers_match_geometry(
        x in 0u16..=512,
        y in 0u16..=512,
        width in 0u16..=128,
        height in 0u16..=128,
        point_x in 0u16..=768,
        point_y in 0u16..=768,
    ) {
        let maybe_rect = FloatingRect::try_new(x, y, width, height);
        prop_assert_eq!(maybe_rect.is_some(), width != 0 && height != 0);

        if let Some(rect) = maybe_rect {
            let position = PanePosition::Floating(rect);
            prop_assert!(position.is_floating());
            prop_assert!(!position.is_tiled());
            prop_assert_eq!(position.rect(), Some(rect));
            prop_assert_eq!(rect.right(), u32::from(x) + u32::from(width));
            prop_assert_eq!(rect.bottom(), u32::from(y) + u32::from(height));
            prop_assert_eq!(
                rect.contains(point_x, point_y),
                point_x >= x
                    && u32::from(point_x) < rect.right()
                    && point_y >= y
                    && u32::from(point_y) < rect.bottom(),
            );
        }

        prop_assert!(PanePosition::Tiled.is_tiled());
        prop_assert!(!PanePosition::Tiled.is_floating());
        prop_assert_eq!(PanePosition::Tiled.rect(), None);
    }

    #[test]
    fn proptest_floating_panes_overlap_is_symmetric_and_edges_do_not_overlap(
        rect in arb_rect(),
        other in arb_rect(),
    ) {
        prop_assert_eq!(rect.overlaps(&other), other.overlaps(&rect));

        let touching_right = FloatingRect::new(
            rect.right().min(u32::from(u16::MAX)) as u16,
            rect.y,
            rect.width,
            rect.height,
        );
        let touching_bottom = FloatingRect::new(
            rect.x,
            rect.bottom().min(u32::from(u16::MAX)) as u16,
            rect.width,
            rect.height,
        );
        prop_assert!(!rect.overlaps(&touching_right));
        prop_assert!(!rect.overlaps(&touching_bottom));
    }

    #[test]
    fn proptest_floating_panes_z_stack_contains_unique_ids_in_ascending_z_order(
        ids in prop::collection::vec(any::<PaneId>(), 0..64),
        focus in any::<PaneId>(),
    ) {
        let mut stack = FloatingZStack::new();
        let mut unique = BTreeSet::new();
        for id in &ids {
            stack.insert_top(*id);
            unique.insert(*id);
        }

        prop_assert_eq!(stack.len(), unique.len());
        prop_assert_eq!(stack.is_empty(), unique.is_empty());
        prop_assert!(z_orders_are_strictly_ascending(&stack));
        for id in &unique {
            prop_assert!(stack.z_of(*id).is_some());
        }

        let before = order(&stack);
        stack.raise_to_top(focus);
        stack.lower_to_bottom(focus);
        stack.raise(focus);
        stack.lower(focus);
        prop_assert_eq!(stack.len(), unique.len());
        prop_assert!(z_orders_are_strictly_ascending(&stack));
        if !unique.contains(&focus) {
            prop_assert_eq!(order(&stack), before);
            prop_assert_eq!(stack.z_of(focus), None);
        }

        stack.remove(focus);
        stack.remove(focus);
        prop_assert_eq!(stack.z_of(focus), None);
        prop_assert!(z_orders_are_strictly_ascending(&stack));
    }

    #[test]
    fn proptest_floating_panes_drag_resize_commit_and_cancel_restore_expected_rects(
        pane in any::<PaneId>(),
        original in arb_rect(),
        updated in arb_rect(),
        handle in arb_resize_handle(),
    ) {
        let mut drag = DragResizeState::default();
        prop_assert!(drag.is_idle());
        prop_assert!(drag.begin_drag(pane, original));
        prop_assert!(!drag.begin_resize(pane, handle, original));
        prop_assert_eq!(drag.pane(), Some(pane));
        prop_assert_eq!(drag.original_rect(), Some(original));
        prop_assert!(drag.update(updated));
        prop_assert_eq!(drag.current_rect(), Some(updated));
        prop_assert_eq!(drag.commit(), Some(updated));
        prop_assert!(drag.is_idle());
        prop_assert!(!drag.update(original));

        let mut resize = DragResizeState::default();
        prop_assert!(resize.begin_resize(pane, handle, original));
        prop_assert!(!resize.begin_drag(pane, updated));
        prop_assert!(resize.update(updated));
        prop_assert_eq!(resize.cancel(), Some(original));
        prop_assert!(resize.is_idle());
    }

    #[test]
    fn proptest_floating_panes_snap_targets_and_applied_rects_stay_on_screen(
        rect in arb_rect(),
        screen_width in 1u16..=1024,
        screen_height in 1u16..=1024,
        snap_distance in 0u16..=16,
        edge in arb_snap_edge(),
    ) {
        let target = snap_target(rect, screen_width, screen_height, snap_distance);
        if let Some(target) = target {
            let snapped = apply_snap(rect, target, screen_width, screen_height);
            prop_assert!(snapped.width > 0);
            prop_assert!(snapped.height > 0);
            prop_assert!(snapped.right() <= u32::from(screen_width));
            prop_assert!(snapped.bottom() <= u32::from(screen_height));
        }

        let snapped = apply_snap(rect, edge, screen_width, screen_height);
        prop_assert!(snapped.width > 0);
        prop_assert!(snapped.height > 0);
        prop_assert!(snapped.right() <= u32::from(screen_width));
        prop_assert!(snapped.bottom() <= u32::from(screen_height));
    }

    #[test]
    fn proptest_floating_panes_a11y_message_is_lossless_payload_construction(
        pane in any::<PaneId>(),
        position in arb_rect(),
        z in any::<u32>(),
        kind in prop_oneof![
            Just(FloatingPaneA11yKind::FocusGained),
            Just(FloatingPaneA11yKind::RectChanged),
            Just(FloatingPaneA11yKind::ZOrderChanged),
            Just(FloatingPaneA11yKind::PinnedToTiled),
            Just(FloatingPaneA11yKind::UnpinnedToFloating),
        ],
    ) {
        let z_order = ZOrder(z);
        let message = make_a11y_message(pane, position, z_order, kind);

        prop_assert_eq!(message.pane, pane);
        prop_assert_eq!(message.position, position);
        prop_assert_eq!(message.z_order, z_order);
        prop_assert_eq!(message.kind, kind);
    }
}
