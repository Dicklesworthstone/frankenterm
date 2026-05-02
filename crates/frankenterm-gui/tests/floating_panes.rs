use frankenterm_core::floating_panes::{
    FloatingPaneA11yKind, FloatingRect, KeyboardCommand, PanePosition, ResizeHandle,
};
use frankenterm_gui::floating_panes::{
    FloatingPaneHitRegion, GuiFloatingPaneController, RobotFloatingPaneCommand,
    classify_hit_region, high_contrast_border_style,
};

fn r(x: u16, y: u16, width: u16, height: u16) -> FloatingRect {
    FloatingRect::new(x, y, width, height)
}

#[test]
fn hit_test_prefers_topmost_overlapping_pane() {
    let mut controller = GuiFloatingPaneController::new();
    controller.set_floating(1, r(0, 0, 10, 5));
    controller.set_floating(2, r(2, 1, 10, 5));

    let hit = controller.hit_test_top_down(3, 2).expect("hit");

    assert_eq!(hit.pane_id, 2);
    assert_eq!(hit.region, FloatingPaneHitRegion::Body);
}

#[test]
fn hit_regions_classify_title_bar_edges_and_body() {
    let rect = r(10, 5, 20, 10);

    assert_eq!(
        classify_hit_region(rect, 10, 5),
        FloatingPaneHitRegion::Resize(ResizeHandle::TopLeft)
    );
    assert_eq!(
        classify_hit_region(rect, 15, 5),
        FloatingPaneHitRegion::TitleBar
    );
    assert_eq!(
        classify_hit_region(rect, 29, 14),
        FloatingPaneHitRegion::Resize(ResizeHandle::BottomRight)
    );
    assert_eq!(
        classify_hit_region(rect, 15, 8),
        FloatingPaneHitRegion::Body
    );
}

#[test]
fn drag_cancel_restores_original_rect() {
    let mut controller = GuiFloatingPaneController::new();
    controller.set_floating(7, r(1, 1, 8, 4));

    assert!(controller.begin_drag(7));
    assert!(controller.update_preview(r(5, 6, 8, 4)));
    assert_eq!(controller.cancel_drag_or_resize(), Some(r(1, 1, 8, 4)));
    assert_eq!(
        controller.pane(7).unwrap().position.rect(),
        Some(r(1, 1, 8, 4))
    );
}

#[test]
fn keyboard_move_resize_snap_and_z_order_use_core_substrate() {
    let mut controller = GuiFloatingPaneController::new();
    controller.set_floating(1, r(10, 10, 10, 5));
    controller.set_floating(2, r(12, 12, 10, 5));
    controller.focus(1);

    controller.apply_keyboard_command(KeyboardCommand::MoveLeft, 80, 24);
    controller.apply_keyboard_command(KeyboardCommand::GrowVertical, 80, 24);
    assert_eq!(
        controller.pane(1).unwrap().position.rect(),
        Some(r(9, 10, 10, 6))
    );

    controller.apply_keyboard_command(KeyboardCommand::RaiseToTop, 80, 24);
    assert_eq!(controller.render_order_back_to_front(), vec![2, 1]);

    controller.apply_keyboard_command(KeyboardCommand::SnapLeft, 80, 24);
    assert_eq!(
        controller.pane(1).unwrap().position.rect(),
        Some(r(0, 0, 40, 24))
    );
}

#[test]
fn layout_snapshot_restore_preserves_relative_order() {
    let mut controller = GuiFloatingPaneController::new();
    controller.set_floating(1, r(0, 0, 10, 5));
    controller.set_floating(2, r(5, 5, 10, 5));
    controller.apply_keyboard_command(KeyboardCommand::LowerToBottom, 80, 24);
    let snapshot = controller.snapshot_layout();

    let mut restored = GuiFloatingPaneController::new();
    restored.restore_layout(&snapshot);

    assert_eq!(
        restored.render_order_back_to_front(),
        controller.render_order_back_to_front()
    );
    assert_eq!(restored.snapshot_layout().len(), 2);
}

#[test]
fn robot_commands_set_and_pin_floating_state() {
    let mut controller = GuiFloatingPaneController::new();

    assert!(
        controller.apply_robot_command(RobotFloatingPaneCommand::SetFloating {
            pane_id: 42,
            rect: r(3, 4, 20, 6),
        })
    );
    assert_eq!(
        controller.pane(42).unwrap().position,
        PanePosition::Floating(r(3, 4, 20, 6))
    );
    assert!(controller.apply_robot_command(RobotFloatingPaneCommand::PinToTiled { pane_id: 42 }));
    assert_eq!(controller.pane(42).unwrap().position, PanePosition::Tiled);
}

#[test]
fn a11y_messages_emit_focus_rect_z_and_pin_changes() {
    let mut controller = GuiFloatingPaneController::new();
    controller.set_floating(3, r(1, 1, 10, 4));
    controller.drain_a11y_messages();

    controller.focus(3);
    controller.apply_keyboard_command(KeyboardCommand::MoveDown, 80, 24);
    controller.apply_keyboard_command(KeyboardCommand::RaiseToTop, 80, 24);
    controller.apply_keyboard_command(KeyboardCommand::TogglePin, 80, 24);

    let kinds: Vec<_> = controller
        .drain_a11y_messages()
        .into_iter()
        .map(|message| message.kind)
        .collect();
    assert_eq!(
        kinds,
        vec![
            FloatingPaneA11yKind::FocusGained,
            FloatingPaneA11yKind::RectChanged,
            FloatingPaneA11yKind::ZOrderChanged,
            FloatingPaneA11yKind::PinnedToTiled
        ]
    );
}

#[test]
fn high_contrast_border_uses_two_pixel_stroke() {
    assert_eq!(
        high_contrast_border_style(true, [255, 255, 0, 255]).width_px,
        2
    );
    assert_eq!(
        high_contrast_border_style(false, [255, 255, 0, 255]).width_px,
        1
    );
}
