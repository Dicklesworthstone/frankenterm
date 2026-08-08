use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use smithay_client_toolkit::compositor::SurfaceData;
use smithay_client_toolkit::reexports::csd_frame::{DecorationsFrame, FrameClick};
use smithay_client_toolkit::seat::pointer::{
    PointerData, PointerDataExt, PointerEvent, PointerEventKind, PointerHandler,
};
use wayland_client::backend::ObjectId;
use wayland_client::protocol::wl_pointer::{ButtonState, WlPointer};
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Proxy, QueueHandle};
use wezterm_input_types::MousePress;

use crate::wayland::SurfaceUserData;

use super::copy_and_paste::CopyAndPaste;
use super::drag_and_drop::DragAndDrop;
use super::state::{clear_surface_authority_if_matches, replace_surface_authority, WaylandState};
use super::WaylandConnection;

impl PointerHandler for WaylandState {
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        pointer: &WlPointer,
        events: &[PointerEvent],
    ) {
        let mut pending_window_ids = PendingPointerWindows::default();

        for evt in events {
            let surface_id = evt.surface.id();
            let authority_event = match evt.kind {
                PointerEventKind::Enter { .. } => PointerAuthorityEvent::Enter,
                PointerEventKind::Leave { .. } => PointerAuthorityEvent::Leave,
                _ => PointerAuthorityEvent::Activity,
            };
            let prior_window_id = self.pointer_window_id;
            let authority_route = {
                let mut active_surface = self.pointer_active_surface_id.borrow_mut();
                route_pointer_authority(&mut *active_surface, &surface_id, authority_event)
            };
            if let Some(retired_surface_id) = authority_route.replaced {
                if let Some(window_id) = self.retire_pending_pointer_surface(&retired_surface_id) {
                    pending_window_ids.insert(window_id);
                }
                if let Some(retired_window_id) = prior_window_id {
                    self.retire_pointer_window_frame(retired_window_id);
                }
            }
            match authority_event {
                PointerAuthorityEvent::Enter => {
                    self.pointer_window_id = pointer_window_id_for_surface(&evt.surface);
                }
                PointerAuthorityEvent::Leave if authority_route.cleared => {
                    self.pointer_window_id = None;
                }
                PointerAuthorityEvent::Leave | PointerAuthorityEvent::Activity => {}
            }
            if let Some(serial) = event_serial(evt) {
                *self.last_serial.borrow_mut() = serial;
            }
            if !authority_route.route_event {
                log::trace!("Ignoring stale Wayland pointer activity for surface {surface_id:?}");
                continue;
            }

            if let Some(pending) = self.surface_to_pending.get(&surface_id) {
                let mut pending = lock_pending_mouse(pending, "pointer_frame");
                if pending.queue(evt) {
                    pending_window_ids.insert(pending.window_id);
                }
            }
            self.pointer_window_frame_event(pointer, evt, authority_route.cleared);
        }

        for window_id in pending_window_ids.iter() {
            dispatch_pending_pointer_window(window_id);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PointerAuthorityEvent {
    Enter,
    Leave,
    Activity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PointerAuthorityRoute<T> {
    route_event: bool,
    replaced: Option<T>,
    cleared: bool,
}

fn route_pointer_authority<T: Clone + Eq>(
    active: &mut Option<T>,
    event_surface: &T,
    event: PointerAuthorityEvent,
) -> PointerAuthorityRoute<T> {
    match event {
        PointerAuthorityEvent::Enter => PointerAuthorityRoute {
            route_event: true,
            replaced: replace_surface_authority(active, event_surface.clone()),
            cleared: false,
        },
        PointerAuthorityEvent::Leave => PointerAuthorityRoute {
            // Route a stale Leave to its named surface so old hover/button
            // state can retire, but do not let it clear a newer authority.
            route_event: true,
            replaced: None,
            cleared: clear_surface_authority_if_matches(active, event_surface),
        },
        PointerAuthorityEvent::Activity => PointerAuthorityRoute {
            route_event: active.as_ref() == Some(event_surface),
            replaced: None,
            cleared: false,
        },
    }
}

fn pointer_window_id_for_surface(surface: &WlSurface) -> Option<usize> {
    if let Some(data) = SurfaceUserData::try_from_wl(surface) {
        return Some(data.window_id);
    }
    let parent = surface.data::<SurfaceData>()?.parent_surface()?;
    SurfaceUserData::try_from_wl(parent).map(|data| data.window_id)
}

fn should_route_pointer_frame_leave(
    authority_cleared: bool,
    event_window_id: usize,
    active_window_id: Option<usize>,
) -> bool {
    authority_cleared || active_window_id != Some(event_window_id)
}

#[derive(Debug, Default)]
struct PendingPointerWindows {
    inline: [Option<usize>; 2],
    overflow: Vec<usize>,
}

impl PendingPointerWindows {
    fn insert(&mut self, window_id: usize) {
        if self.iter().any(|existing| existing == window_id) {
            return;
        }
        if let Some(slot) = self.inline.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some(window_id);
        } else {
            self.overflow.push(window_id);
        }
    }

    fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.inline
            .iter()
            .flatten()
            .copied()
            .chain(self.overflow.iter().copied())
    }
}

fn dispatch_pending_pointer_window(window_id: usize) {
    WaylandConnection::with_window_inner(window_id, move |inner| {
        inner.dispatch_pending_mouse();
        Ok(())
    });
}

pub(super) struct PointerUserData {
    pub(super) pdata: PointerData,
    pub(super) state: Mutex<PointerState>,
}

impl PointerUserData {
    pub(super) fn new(seat: WlSeat) -> Self {
        Self {
            pdata: PointerData::new(seat),
            state: Default::default(),
        }
    }
}

#[derive(Default)]
pub(super) struct PointerState {
    pub(super) drag_and_drop: DragAndDrop,
}

impl PointerDataExt for PointerUserData {
    fn pointer_data(&self) -> &PointerData {
        &self.pdata
    }
}

#[derive(Clone, Debug)]
pub struct PendingMouse {
    window_id: usize,
    pub(super) copy_and_paste: Arc<Mutex<CopyAndPaste>>,
    surface_coords: Option<(f64, f64)>,
    button: Vec<(MousePress, ButtonState)>,
    scroll: Option<(f64, f64)>,
    in_window: bool,
}

fn lock_pending_mouse<'a>(
    pending: &'a Arc<Mutex<PendingMouse>>,
    context: &str,
) -> MutexGuard<'a, PendingMouse> {
    match pending.lock() {
        Ok(pending) => pending,
        Err(poisoned) => {
            log::error!("Wayland pending mouse lock was poisoned during {context}");
            poisoned.into_inner()
        }
    }
}

impl PendingMouse {
    pub(super) fn create(
        window_id: usize,
        copy_and_paste: &Arc<Mutex<CopyAndPaste>>,
    ) -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            window_id,
            copy_and_paste: Arc::clone(copy_and_paste),
            button: vec![],
            scroll: None,
            surface_coords: None,
            in_window: false,
        }))
    }

    pub(super) fn queue(&mut self, evt: &PointerEvent) -> bool {
        match evt.kind {
            PointerEventKind::Enter { .. } => {
                self.in_window = true;
                false
            }
            PointerEventKind::Leave { .. } => {
                let changed = self.in_window;
                self.surface_coords = None;
                self.in_window = false;
                changed
            }
            PointerEventKind::Motion { .. } => {
                let changed = self.surface_coords.is_none();
                self.surface_coords.replace(evt.position);
                changed
            }
            PointerEventKind::Press { button, .. } | PointerEventKind::Release { button, .. } => {
                fn linux_button(b: u32) -> Option<MousePress> {
                    // See BTN_LEFT and friends in <linux/input-event-codes.h>
                    match b {
                        0x110 => Some(MousePress::Left),
                        0x111 => Some(MousePress::Right),
                        0x112 => Some(MousePress::Middle),
                        _ => None,
                    }
                }
                let button = match linux_button(button) {
                    Some(button) => button,
                    None => return false,
                };
                let changed = self.button.is_empty();
                let button_state = match evt.kind {
                    PointerEventKind::Press { .. } => ButtonState::Pressed,
                    PointerEventKind::Release { .. } => ButtonState::Released,
                    _ => unreachable!(),
                };
                self.button.push((button, button_state));
                changed
            }
            PointerEventKind::Axis {
                horizontal,
                vertical,
                ..
            } => {
                let changed = self.scroll.is_none();
                let (x, y) = self.scroll.take().unwrap_or((0., 0.));
                self.scroll
                    .replace((x + horizontal.absolute, y + vertical.absolute));
                changed
            }
        }
    }

    pub(super) fn next_button(pending: &Arc<Mutex<Self>>) -> Option<(MousePress, ButtonState)> {
        let mut pending = lock_pending_mouse(pending, "next_button");
        if pending.button.is_empty() {
            None
        } else {
            Some(pending.button.remove(0))
        }
    }

    pub(super) fn coords(pending: &Arc<Mutex<Self>>) -> Option<(f64, f64)> {
        let mut pending = lock_pending_mouse(pending, "coords");
        pending.surface_coords.take()
    }

    pub(super) fn scroll(pending: &Arc<Mutex<Self>>) -> Option<(f64, f64)> {
        let mut pending = lock_pending_mouse(pending, "scroll");
        pending.scroll.take()
    }

    pub(super) fn in_window(pending: &Arc<Mutex<Self>>) -> bool {
        lock_pending_mouse(pending, "in_window").in_window
    }

    fn clear_focus(&mut self) -> bool {
        let changed = self.in_window || self.surface_coords.is_some();
        self.surface_coords = None;
        self.in_window = false;
        changed
    }
}

fn event_serial(event: &PointerEvent) -> Option<u32> {
    Some(match event.kind {
        PointerEventKind::Enter { serial, .. } => serial,
        PointerEventKind::Leave { serial, .. } => serial,
        PointerEventKind::Press { serial, .. } => serial,
        PointerEventKind::Release { serial, .. } => serial,
        _ => return None,
    })
}

impl WaylandState {
    pub(super) fn clear_pointer_focus(&mut self) {
        let retired_surface_id = self.pointer_active_surface_id.get_mut().take();
        let retired_window_id = self.pointer_window_id.take();
        if let Some(retired_surface_id) = retired_surface_id {
            if let Some(window_id) = self.retire_pending_pointer_surface(&retired_surface_id) {
                dispatch_pending_pointer_window(window_id);
            }
        }
        if let Some(retired_window_id) = retired_window_id {
            self.retire_pointer_window_frame(retired_window_id);
        }
    }

    fn retire_pending_pointer_surface(&self, surface_id: &ObjectId) -> Option<usize> {
        let pending = self.surface_to_pending.get(surface_id)?;
        let mut pending = lock_pending_mouse(pending, "pointer focus retirement");
        pending.clear_focus().then_some(pending.window_id)
    }

    fn retire_pointer_window_frame(&self, window_id: usize) {
        let Some(window) = self.window_by_id(window_id) else {
            return;
        };
        window.borrow_mut().window_frame.click_point_left();
    }

    fn pointer_window_frame_event(
        &self,
        pointer: &WlPointer,
        evt: &PointerEvent,
        authority_cleared: bool,
    ) {
        let parent_surface = match evt.surface.data::<SurfaceData>() {
            Some(data) => match data.parent_surface() {
                Some(surface) => surface,
                None => return,
            },
            None => return,
        };
        let Some(surface_data) = SurfaceUserData::try_from_wl(parent_surface) else {
            log::warn!("Wayland pointer frame event referenced an unknown parent surface");
            return;
        };
        let wid = surface_data.window_id;
        if matches!(evt.kind, PointerEventKind::Leave { .. })
            && !should_route_pointer_frame_leave(authority_cleared, wid, self.pointer_window_id)
        {
            // A stale Leave for an older decoration of the currently active
            // window must not clear the hover established by a newer Enter in
            // the same pointer frame.
            return;
        }
        let windows = self.windows.borrow();
        let Some(window) = windows.get(&wid) else {
            log::warn!("Wayland pointer window-frame event referenced missing window {wid}");
            return;
        };
        let mut inner = window.borrow_mut();
        let (x, y) = evt.position;

        match evt.kind {
            PointerEventKind::Enter { .. } => {
                inner
                    .window_frame
                    .click_point_moved(Duration::ZERO, &evt.surface.id(), x, y);
            }
            PointerEventKind::Leave { .. } => {
                inner.window_frame.click_point_left();
            }
            PointerEventKind::Motion { .. } => {
                inner
                    .window_frame
                    .click_point_moved(Duration::ZERO, &evt.surface.id(), x, y);
            }
            PointerEventKind::Press { button, serial, .. }
            | PointerEventKind::Release { button, serial, .. } => {
                let pressed = matches!(evt.kind, PointerEventKind::Press { .. });
                let click = match button {
                    0x110 => FrameClick::Normal,
                    0x111 => FrameClick::Alternate,
                    _ => return,
                };
                if let Some(action) = inner.window_frame.on_click(Duration::ZERO, click, pressed) {
                    inner.frame_action(pointer, serial, action);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        route_pointer_authority, should_route_pointer_frame_leave, PendingPointerWindows,
        PointerAuthorityEvent, PointerAuthorityRoute,
    };

    #[test]
    fn leave_then_enter_in_one_frame_routes_each_named_surface() {
        let mut active = Some(1_u32);

        assert_eq!(
            route_pointer_authority(&mut active, &1, PointerAuthorityEvent::Leave),
            PointerAuthorityRoute {
                route_event: true,
                replaced: None,
                cleared: true,
            }
        );
        assert_eq!(active, None);
        assert_eq!(
            route_pointer_authority(&mut active, &2, PointerAuthorityEvent::Enter),
            PointerAuthorityRoute {
                route_event: true,
                replaced: None,
                cleared: false,
            }
        );
        assert_eq!(active, Some(2));
    }

    #[test]
    fn stale_leave_after_new_enter_does_not_clear_new_pointer_authority() {
        let mut active = Some(1_u32);

        assert_eq!(
            route_pointer_authority(&mut active, &2, PointerAuthorityEvent::Enter).replaced,
            Some(1)
        );
        let stale_leave = route_pointer_authority(&mut active, &1, PointerAuthorityEvent::Leave);
        assert!(stale_leave.route_event);
        assert!(!stale_leave.cleared);
        assert_eq!(active, Some(2));
        assert!(
            !route_pointer_authority(&mut active, &1, PointerAuthorityEvent::Activity).route_event
        );
        assert!(
            route_pointer_authority(&mut active, &2, PointerAuthorityEvent::Activity).route_event
        );
    }

    #[test]
    fn stale_decoration_leave_cannot_clear_new_hover_in_the_same_window() {
        assert!(!should_route_pointer_frame_leave(false, 7, Some(7)));
        assert!(should_route_pointer_frame_leave(false, 7, Some(8)));
        assert!(should_route_pointer_frame_leave(true, 7, None));
    }

    #[test]
    fn pointer_frame_schedules_each_affected_window_once() {
        let mut window_ids = PendingPointerWindows::default();

        window_ids.insert(7);
        window_ids.insert(8);
        window_ids.insert(7);

        assert_eq!(window_ids.iter().collect::<Vec<_>>(), vec![7, 8]);
    }

    #[test]
    fn pointer_frame_overflow_preserves_unusual_multi_window_sequences() {
        let mut window_ids = PendingPointerWindows::default();

        window_ids.insert(7);
        window_ids.insert(8);
        window_ids.insert(9);

        assert_eq!(window_ids.iter().collect::<Vec<_>>(), vec![7, 8, 9]);
    }
}
