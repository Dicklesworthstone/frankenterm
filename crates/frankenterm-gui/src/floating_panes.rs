use frankenterm_core::a11y_tree::{AccessibilityEvent, AnnouncePriority};
use frankenterm_core::floating_panes::{
    DEFAULT_SNAP_DISTANCE, DragResizeState, FloatingPaneA11yKind, FloatingPaneA11yMessage,
    FloatingRect, FloatingZStack, KeyboardCommand, PaneId, PanePosition, ResizeHandle, ZOrder,
    apply_snap, make_a11y_message, snap_target,
};
use frankenterm_core::smart_selection_a11y_recorder::RecorderHandle;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuiFloatingPane {
    pub pane_id: PaneId,
    pub position: PanePosition,
    pub z_order: Option<ZOrder>,
}

impl GuiFloatingPane {
    #[must_use]
    pub const fn tiled(pane_id: PaneId) -> Self {
        Self {
            pane_id,
            position: PanePosition::Tiled,
            z_order: None,
        }
    }

    #[must_use]
    pub const fn floating(pane_id: PaneId, rect: FloatingRect, z_order: ZOrder) -> Self {
        Self {
            pane_id,
            position: PanePosition::Floating(rect),
            z_order: Some(z_order),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FloatingPaneHitRegion {
    Body,
    TitleBar,
    Resize(ResizeHandle),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FloatingPaneHit {
    pub pane_id: PaneId,
    pub rect: FloatingRect,
    pub z_order: ZOrder,
    pub region: FloatingPaneHitRegion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FloatingLayoutEntry {
    pub pane_id: PaneId,
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    pub z_order: u32,
}

impl FloatingLayoutEntry {
    #[must_use]
    pub const fn rect(&self) -> Option<FloatingRect> {
        FloatingRect::try_new(self.x, self.y, self.width, self.height)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FloatingPaneBorderStyle {
    pub width_px: u8,
    pub color: [u8; 4],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RobotFloatingPaneCommand {
    SetFloating { pane_id: PaneId, rect: FloatingRect },
    PinToTiled { pane_id: PaneId },
}

#[derive(Debug, Default)]
pub struct GuiFloatingPaneController {
    panes: HashMap<PaneId, GuiFloatingPane>,
    zstack: FloatingZStack,
    drag_resize: DragResizeState,
    focused: Option<PaneId>,
    a11y_messages: Vec<FloatingPaneA11yMessage>,
}

impl GuiFloatingPaneController {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn pane(&self, pane_id: PaneId) -> Option<GuiFloatingPane> {
        self.panes.get(&pane_id).copied()
    }

    #[must_use]
    pub fn focused(&self) -> Option<PaneId> {
        self.focused
    }

    #[must_use]
    pub fn floating_len(&self) -> usize {
        self.zstack.len()
    }

    pub fn set_floating(&mut self, pane_id: PaneId, rect: FloatingRect) -> ZOrder {
        let z_order = self.zstack.insert_top(pane_id);
        self.panes
            .insert(pane_id, GuiFloatingPane::floating(pane_id, rect, z_order));
        self.focused = Some(pane_id);
        self.push_a11y(pane_id, FloatingPaneA11yKind::UnpinnedToFloating);
        z_order
    }

    /// Restore a pane from authoritative mux state without inventing focus or
    /// emitting a user-action announcement.
    pub fn restore_floating(&mut self, pane_id: PaneId, rect: FloatingRect) -> ZOrder {
        let z_order = self.zstack.insert_top(pane_id);
        self.panes
            .insert(pane_id, GuiFloatingPane::floating(pane_id, rect, z_order));
        z_order
    }

    /// Restore authoritative focus without announcing a new focus action.
    ///
    /// `None` is meaningful: a tab may have visible floating panes while its
    /// tiled pane owns focus.
    pub fn restore_focus(&mut self, pane_id: Option<PaneId>) -> bool {
        if pane_id.is_some_and(|pane_id| self.rect_for(pane_id).is_none()) {
            return false;
        }
        self.focused = pane_id;
        true
    }

    /// Reconcile a speculative controller rect with the geometry actually
    /// committed by the mux. This is intentionally silent; the integration
    /// layer decides whether the authoritative before/after state warrants an
    /// accessibility announcement.
    pub fn reconcile_committed_rect(&mut self, pane_id: PaneId, rect: FloatingRect) -> bool {
        self.set_rect(pane_id, rect).is_some()
    }

    /// Announce the controller's current authoritative rectangle.
    pub fn announce_rect_changed(&mut self, pane_id: PaneId) {
        self.push_a11y(pane_id, FloatingPaneA11yKind::RectChanged);
    }

    pub fn pin_to_tiled(&mut self, pane_id: PaneId) -> bool {
        let Some(rect) = self.rect_for(pane_id) else {
            return false;
        };
        let z_order = self.zstack.z_of(pane_id).unwrap_or_default();
        let Some(pane) = self.panes.get_mut(&pane_id) else {
            return false;
        };
        if pane.position.is_tiled() {
            return false;
        }
        pane.position = PanePosition::Tiled;
        pane.z_order = None;
        self.zstack.remove(pane_id);
        if self.focused == Some(pane_id) {
            self.focused = None;
        }
        self.a11y_messages.push(make_a11y_message(
            pane_id,
            rect,
            z_order,
            FloatingPaneA11yKind::PinnedToTiled,
        ));
        true
    }

    #[must_use]
    pub fn render_order_back_to_front(&self) -> Vec<PaneId> {
        self.zstack
            .iter_back_to_front()
            .map(|(pane_id, _)| pane_id)
            .collect()
    }

    #[must_use]
    pub fn hit_test_top_down(&self, x: u16, y: u16) -> Option<FloatingPaneHit> {
        self.zstack
            .iter_back_to_front()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .find_map(|(pane_id, z_order)| {
                let rect = self.rect_for(pane_id)?;
                rect.contains(x, y).then(|| FloatingPaneHit {
                    pane_id,
                    rect,
                    z_order,
                    region: classify_hit_region(rect, x, y),
                })
            })
    }

    pub fn focus(&mut self, pane_id: PaneId) -> bool {
        if self.rect_for(pane_id).is_none() {
            return false;
        }
        if self.focused == Some(pane_id) {
            return false;
        }
        self.focused = Some(pane_id);
        self.push_a11y(pane_id, FloatingPaneA11yKind::FocusGained);
        true
    }

    pub fn begin_drag(&mut self, pane_id: PaneId) -> bool {
        let Some(rect) = self.rect_for(pane_id) else {
            return false;
        };
        self.drag_resize.begin_drag(pane_id, rect)
    }

    pub fn begin_resize(&mut self, pane_id: PaneId, handle: ResizeHandle) -> bool {
        let Some(rect) = self.rect_for(pane_id) else {
            return false;
        };
        self.drag_resize.begin_resize(pane_id, handle, rect)
    }

    pub fn update_preview(&mut self, rect: FloatingRect) -> bool {
        self.drag_resize.update(rect)
    }

    pub fn commit_drag_or_resize(&mut self) -> Option<FloatingRect> {
        let pane_id = self.drag_resize.pane()?;
        let rect = self.drag_resize.commit()?;
        self.set_rect(pane_id, rect)?;
        self.push_a11y(pane_id, FloatingPaneA11yKind::RectChanged);
        Some(rect)
    }

    pub fn cancel_drag_or_resize(&mut self) -> Option<FloatingRect> {
        let pane_id = self.drag_resize.pane()?;
        let rect = self.drag_resize.cancel()?;
        self.set_rect(pane_id, rect)?;
        Some(rect)
    }

    pub fn cycle_overlapping_at(&mut self, x: u16, y: u16) -> Option<PaneId> {
        let next = self
            .zstack
            .cycle_among_overlapping(self.focused, x, y, |pane_id| self.rect_for(pane_id))?;
        if self.focused == Some(next) || !self.focus(next) {
            return None;
        }
        Some(next)
    }

    pub fn apply_keyboard_command(
        &mut self,
        command: KeyboardCommand,
        screen_width: u16,
        screen_height: u16,
    ) -> Option<PanePosition> {
        if command == KeyboardCommand::CancelOperation {
            return self.cancel_drag_or_resize().map(PanePosition::Floating);
        }

        let pane_id = self.focused?;
        let rect = self.rect_for(pane_id)?;
        match command {
            KeyboardCommand::MoveLeft => self.commit_rect(pane_id, move_rect(rect, -1, 0)),
            KeyboardCommand::MoveRight => self.commit_rect(pane_id, move_rect(rect, 1, 0)),
            KeyboardCommand::MoveUp => self.commit_rect(pane_id, move_rect(rect, 0, -1)),
            KeyboardCommand::MoveDown => self.commit_rect(pane_id, move_rect(rect, 0, 1)),
            KeyboardCommand::GrowHorizontal => self.commit_rect(pane_id, resize_rect(rect, 1, 0)),
            KeyboardCommand::ShrinkHorizontal => {
                self.commit_rect(pane_id, resize_rect(rect, -1, 0))
            }
            KeyboardCommand::GrowVertical => self.commit_rect(pane_id, resize_rect(rect, 0, 1)),
            KeyboardCommand::ShrinkVertical => self.commit_rect(pane_id, resize_rect(rect, 0, -1)),
            KeyboardCommand::SnapTop
            | KeyboardCommand::SnapBottom
            | KeyboardCommand::SnapLeft
            | KeyboardCommand::SnapRight => {
                let edge = match command {
                    KeyboardCommand::SnapTop => frankenterm_core::floating_panes::SnapEdge::Top,
                    KeyboardCommand::SnapBottom => {
                        frankenterm_core::floating_panes::SnapEdge::Bottom
                    }
                    KeyboardCommand::SnapLeft => frankenterm_core::floating_panes::SnapEdge::Left,
                    KeyboardCommand::SnapRight => frankenterm_core::floating_panes::SnapEdge::Right,
                    _ => unreachable!(),
                };
                self.commit_rect(pane_id, apply_snap(rect, edge, screen_width, screen_height))
            }
            KeyboardCommand::TogglePin => self.pin_to_tiled(pane_id).then_some(PanePosition::Tiled),
            KeyboardCommand::RaiseOne => {
                if !self.zstack.raise(pane_id) {
                    return None;
                }
                self.refresh_z_order(pane_id);
                self.push_a11y(pane_id, FloatingPaneA11yKind::ZOrderChanged);
                self.pane(pane_id).map(|pane| pane.position)
            }
            KeyboardCommand::LowerOne => {
                if !self.zstack.lower(pane_id) {
                    return None;
                }
                self.refresh_z_order(pane_id);
                self.push_a11y(pane_id, FloatingPaneA11yKind::ZOrderChanged);
                self.pane(pane_id).map(|pane| pane.position)
            }
            KeyboardCommand::RaiseToTop => {
                if !self.zstack.raise_to_top(pane_id) {
                    return None;
                }
                self.refresh_z_order(pane_id);
                self.push_a11y(pane_id, FloatingPaneA11yKind::ZOrderChanged);
                self.pane(pane_id).map(|pane| pane.position)
            }
            KeyboardCommand::LowerToBottom => {
                if !self.zstack.lower_to_bottom(pane_id) {
                    return None;
                }
                self.refresh_z_order(pane_id);
                self.push_a11y(pane_id, FloatingPaneA11yKind::ZOrderChanged);
                self.pane(pane_id).map(|pane| pane.position)
            }
            KeyboardCommand::CycleOverlapping => None,
            KeyboardCommand::CancelOperation => unreachable!(),
        }
    }

    #[must_use]
    pub fn snap_preview(
        &self,
        draft_rect: FloatingRect,
        screen_width: u16,
        screen_height: u16,
    ) -> Option<FloatingRect> {
        snap_target(
            draft_rect,
            screen_width,
            screen_height,
            DEFAULT_SNAP_DISTANCE,
        )
        .map(|edge| apply_snap(draft_rect, edge, screen_width, screen_height))
    }

    #[must_use]
    pub fn snapshot_layout(&self) -> Vec<FloatingLayoutEntry> {
        self.zstack
            .iter_back_to_front()
            .filter_map(|(pane_id, z_order)| {
                let rect = self.rect_for(pane_id)?;
                Some(FloatingLayoutEntry {
                    pane_id,
                    x: rect.x,
                    y: rect.y,
                    width: rect.width,
                    height: rect.height,
                    z_order: z_order.0,
                })
            })
            .collect()
    }

    pub fn restore_layout(&mut self, entries: &[FloatingLayoutEntry]) {
        let mut entries = entries.to_vec();
        entries.sort_by_key(|entry| entry.z_order);
        for entry in entries {
            if let Some(rect) = entry.rect() {
                self.restore_floating(entry.pane_id, rect);
            }
        }
    }

    pub fn apply_robot_command(&mut self, command: RobotFloatingPaneCommand) -> bool {
        match command {
            RobotFloatingPaneCommand::SetFloating { pane_id, rect } => {
                self.set_floating(pane_id, rect);
                true
            }
            RobotFloatingPaneCommand::PinToTiled { pane_id } => self.pin_to_tiled(pane_id),
        }
    }

    pub fn drain_a11y_messages(&mut self) -> Vec<FloatingPaneA11yMessage> {
        std::mem::take(&mut self.a11y_messages)
    }

    fn rect_for(&self, pane_id: PaneId) -> Option<FloatingRect> {
        self.panes.get(&pane_id)?.position.rect()
    }

    fn set_rect(&mut self, pane_id: PaneId, rect: FloatingRect) -> Option<()> {
        let pane = self.panes.get_mut(&pane_id)?;
        pane.position = PanePosition::Floating(rect);
        Some(())
    }

    fn commit_rect(&mut self, pane_id: PaneId, rect: FloatingRect) -> Option<PanePosition> {
        if self.rect_for(pane_id) == Some(rect) {
            return None;
        }
        self.set_rect(pane_id, rect)?;
        self.push_a11y(pane_id, FloatingPaneA11yKind::RectChanged);
        Some(PanePosition::Floating(rect))
    }

    fn refresh_z_order(&mut self, pane_id: PaneId) {
        if let Some(pane) = self.panes.get_mut(&pane_id) {
            pane.z_order = self.zstack.z_of(pane_id);
        }
    }

    fn push_a11y(&mut self, pane_id: PaneId, kind: FloatingPaneA11yKind) {
        let Some(rect) = self.rect_for(pane_id) else {
            return;
        };
        let z_order = self.zstack.z_of(pane_id).unwrap_or_default();
        self.a11y_messages
            .push(make_a11y_message(pane_id, rect, z_order, kind));
    }
}

#[must_use]
pub fn classify_hit_region(rect: FloatingRect, x: u16, y: u16) -> FloatingPaneHitRegion {
    let on_left = x == rect.x;
    let on_right = u32::from(x) + 1 == rect.right();
    let on_top = y == rect.y;
    let on_bottom = u32::from(y) + 1 == rect.bottom();

    match (on_top, on_bottom, on_left, on_right) {
        (true, _, true, _) => FloatingPaneHitRegion::Resize(ResizeHandle::TopLeft),
        (true, _, _, true) => FloatingPaneHitRegion::Resize(ResizeHandle::TopRight),
        (_, true, true, _) => FloatingPaneHitRegion::Resize(ResizeHandle::BottomLeft),
        (_, true, _, true) => FloatingPaneHitRegion::Resize(ResizeHandle::BottomRight),
        (true, false, false, false) => FloatingPaneHitRegion::TitleBar,
        (false, true, false, false) => FloatingPaneHitRegion::Resize(ResizeHandle::Bottom),
        (false, false, true, false) => FloatingPaneHitRegion::Resize(ResizeHandle::Left),
        (false, false, false, true) => FloatingPaneHitRegion::Resize(ResizeHandle::Right),
        _ => FloatingPaneHitRegion::Body,
    }
}

#[must_use]
pub fn mux_pane_id_to_floating_pane_id(pane_id: mux::pane::PaneId) -> Option<PaneId> {
    PaneId::try_from(pane_id).ok()
}

#[must_use]
pub fn floating_pane_id_to_mux_pane_id(pane_id: PaneId) -> Option<mux::pane::PaneId> {
    mux::pane::PaneId::try_from(pane_id).ok()
}

#[must_use]
pub const fn high_contrast_border_style(
    high_contrast: bool,
    border_color: [u8; 4],
) -> FloatingPaneBorderStyle {
    FloatingPaneBorderStyle {
        width_px: if high_contrast { 2 } else { 1 },
        color: border_color,
    }
}

#[must_use]
pub fn floating_pane_a11y_value(message: &FloatingPaneA11yMessage) -> String {
    let rect = message.position;
    match message.kind {
        FloatingPaneA11yKind::FocusGained => format!(
            "Floating pane {} focused, position {},{}, size {}x{}, z-order {}",
            message.pane, rect.x, rect.y, rect.width, rect.height, message.z_order.0
        ),
        FloatingPaneA11yKind::RectChanged => format!(
            "Floating pane {} moved to {},{}, size {}x{}",
            message.pane, rect.x, rect.y, rect.width, rect.height
        ),
        FloatingPaneA11yKind::ZOrderChanged => format!(
            "Floating pane {} z-order {}",
            message.pane, message.z_order.0
        ),
        FloatingPaneA11yKind::PinnedToTiled => {
            format!("Floating pane {} pinned to grid", message.pane)
        }
        FloatingPaneA11yKind::UnpinnedToFloating => {
            format!("Pane {} floating", message.pane)
        }
    }
}

#[must_use]
pub fn floating_pane_a11y_event(
    message: &FloatingPaneA11yMessage,
    ts_ms: u64,
    priority: AnnouncePriority,
) -> AccessibilityEvent {
    AccessibilityEvent::AnnounceMessage {
        ts_ms,
        priority,
        value: floating_pane_a11y_value(message),
    }
}

pub fn shared_floating_pane_recorder() -> &'static RecorderHandle {
    static SHARED: OnceLock<RecorderHandle> = OnceLock::new();
    SHARED.get_or_init(RecorderHandle::default)
}

pub fn emit_floating_pane_a11y_messages(messages: &[FloatingPaneA11yMessage]) {
    use std::time::{SystemTime, UNIX_EPOCH};

    let ts_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    let recorder = shared_floating_pane_recorder();
    for message in messages {
        recorder.record(floating_pane_a11y_event(
            message,
            ts_ms,
            AnnouncePriority::Polite,
        ));
    }
}

fn move_rect(rect: FloatingRect, dx: i16, dy: i16) -> FloatingRect {
    let x = i32::from(rect.x)
        .saturating_add(i32::from(dx))
        .clamp(0, i32::from(u16::MAX));
    let y = i32::from(rect.y)
        .saturating_add(i32::from(dy))
        .clamp(0, i32::from(u16::MAX));
    let x = u16::try_from(x).expect("clamped floating-pane x coordinate must fit in u16");
    let y = u16::try_from(y).expect("clamped floating-pane y coordinate must fit in u16");
    FloatingRect::new(x, y, rect.width, rect.height)
}

fn resize_rect(rect: FloatingRect, dw: i16, dh: i16) -> FloatingRect {
    let width = i32::from(rect.width)
        .saturating_add(i32::from(dw))
        .clamp(1, i32::from(u16::MAX));
    let height = i32::from(rect.height)
        .saturating_add(i32::from(dh))
        .clamp(1, i32::from(u16::MAX));
    let width = u16::try_from(width).expect("clamped floating-pane width must fit in u16");
    let height = u16::try_from(height).expect("clamped floating-pane height must fit in u16");
    FloatingRect::new(rect.x, rect.y, width, height)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn move_rect_saturates_at_coordinate_bounds() {
        let rect = FloatingRect::new(u16::MAX, u16::MAX, 1, 1);

        assert_eq!(move_rect(rect, 1, 1), rect);
        assert_eq!(
            move_rect(FloatingRect::new(0, 0, 1, 1), -1, -1),
            FloatingRect::new(0, 0, 1, 1)
        );
    }

    #[test]
    fn resize_rect_saturates_at_nonzero_dimension_bounds() {
        let maximum = FloatingRect::new(0, 0, u16::MAX, u16::MAX);
        assert_eq!(resize_rect(maximum, 1, 1), maximum);

        let minimum = FloatingRect::new(0, 0, 1, 1);
        assert_eq!(resize_rect(minimum, -1, -1), minimum);
    }

    #[test]
    fn keyboard_growth_at_maximum_dimensions_is_a_silent_noop() {
        let mut controller = GuiFloatingPaneController::new();
        let pane_id = 7;
        let maximum = FloatingRect::new(0, 0, u16::MAX, u16::MAX);
        controller.restore_floating(pane_id, maximum);
        assert!(controller.restore_focus(Some(pane_id)));

        assert_eq!(
            controller.apply_keyboard_command(KeyboardCommand::GrowHorizontal, u16::MAX, u16::MAX,),
            None
        );
        assert_eq!(
            controller.apply_keyboard_command(KeyboardCommand::GrowVertical, u16::MAX, u16::MAX,),
            None
        );
        assert!(controller.drain_a11y_messages().is_empty());
    }

    #[test]
    fn single_pane_focus_z_order_cycle_and_idle_cancel_are_silent_noops() {
        let mut controller = GuiFloatingPaneController::new();
        let pane_id = 11;
        controller.restore_floating(pane_id, FloatingRect::new(0, 0, 10, 10));
        assert!(controller.restore_focus(Some(pane_id)));

        assert!(!controller.focus(pane_id));
        assert_eq!(controller.cycle_overlapping_at(5, 5), None);
        assert_eq!(
            controller.apply_keyboard_command(KeyboardCommand::RaiseOne, 80, 24),
            None
        );
        assert_eq!(
            controller.apply_keyboard_command(KeyboardCommand::LowerOne, 80, 24),
            None
        );
        assert_eq!(
            controller.apply_keyboard_command(KeyboardCommand::RaiseToTop, 80, 24),
            None
        );
        assert_eq!(
            controller.apply_keyboard_command(KeyboardCommand::LowerToBottom, 80, 24),
            None
        );
        assert_eq!(
            controller.apply_keyboard_command(KeyboardCommand::CancelOperation, 80, 24),
            None
        );
        assert!(controller.drain_a11y_messages().is_empty());
    }

    #[test]
    fn authoritative_restore_does_not_invent_focus_or_announcements() {
        let mut controller = GuiFloatingPaneController::new();
        controller.restore_floating(1, FloatingRect::new(0, 0, 10, 10));
        controller.restore_floating(2, FloatingRect::new(10, 0, 10, 10));

        assert_eq!(controller.focused(), None);
        assert!(controller.drain_a11y_messages().is_empty());
        assert!(controller.restore_focus(Some(1)));
        assert_eq!(controller.focused(), Some(1));
        assert!(controller.restore_focus(None));
        assert_eq!(controller.focused(), None);
        assert!(!controller.restore_focus(Some(99)));
        assert_eq!(controller.focused(), None);
    }

    #[test]
    fn committed_rect_reconciliation_is_silent_until_explicit_announcement() {
        let mut controller = GuiFloatingPaneController::new();
        controller.restore_floating(1, FloatingRect::new(0, 0, 10, 10));
        let committed = FloatingRect::new(2, 3, 5, 4);

        assert!(controller.reconcile_committed_rect(1, committed));
        assert_eq!(
            controller.pane(1).and_then(|pane| pane.position.rect()),
            Some(committed)
        );
        assert!(controller.drain_a11y_messages().is_empty());

        controller.announce_rect_changed(1);
        let messages = controller.drain_a11y_messages();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].position, committed);
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn pane_id_above_u32_max_round_trips_through_gui_bridge() {
        let pane_id = u64::from(u32::MAX) + 1;
        let mux_pane_id = usize::try_from(pane_id).expect("64-bit mux pane id");

        assert_eq!(mux_pane_id_to_floating_pane_id(mux_pane_id), Some(pane_id));
        assert_eq!(floating_pane_id_to_mux_pane_id(pane_id), Some(mux_pane_id));
    }
}
