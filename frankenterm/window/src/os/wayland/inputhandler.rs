//! Implements zwp_text_input_v3 for handling IME
use std::borrow::Borrow;
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, MutexGuard};

use smithay_client_toolkit::globals::GlobalData;
use wayland_client::backend::ObjectId;
use wayland_client::globals::{BindError, GlobalList};
use wayland_client::protocol::wl_keyboard::WlKeyboard;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Dispatch, Proxy, QueueHandle};
use wayland_protocols::wp::text_input::zv3::client::zwp_text_input_manager_v3::ZwpTextInputManagerV3;
use wayland_protocols::wp::text_input::zv3::client::zwp_text_input_v3::{
    Event as TextInputEvent, ZwpTextInputV3,
};
use wezterm_input_types::{KeyCode, KeyEvent, KeyboardLedStatus, Modifiers};

use crate::{DeadKeyStatus, WindowEvent};

use super::state::WaylandState;

#[derive(Clone, Default, Debug)]
struct PendingState {
    pre_edit: Option<String>,
    commit: Option<String>,
}

pub(super) struct TextInputState {
    text_input_manager: ZwpTextInputManagerV3,
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    input_by_seat: HashMap<ObjectId, ZwpTextInputV3>,
    keyboard_to_seat: HashMap<ObjectId, ObjectId>,
    surface_to_keyboard: HashMap<ObjectId, ObjectId>,
    pending_state: HashMap<ObjectId, PendingState>,
}

impl TextInputState {
    fn lock_inner(&self, context: &str) -> Option<MutexGuard<'_, Inner>> {
        match self.inner.lock() {
            Ok(inner) => Some(inner),
            Err(_) => {
                log::error!("Wayland text input state lock was poisoned during {context}");
                None
            }
        }
    }

    pub(super) fn bind(
        globals: &GlobalList,
        queue_handle: &QueueHandle<WaylandState>,
    ) -> Result<Self, BindError> {
        let text_input_manager = globals.bind(queue_handle, 1..=1, GlobalData)?;
        Ok(Self {
            text_input_manager,
            inner: Mutex::new(Inner::default()),
        })
    }

    pub fn get_text_input_for_keyboard(&self, keyboard: &WlKeyboard) -> Option<ZwpTextInputV3> {
        let inner = self.lock_inner("keyboard lookup")?;
        let keyboard_id = keyboard.id();
        let seat_id = inner.keyboard_to_seat.get(&keyboard_id)?;
        inner.input_by_seat.get(&seat_id).cloned()
    }

    pub(super) fn get_text_input_for_surface(&self, surface: &WlSurface) -> Option<ZwpTextInputV3> {
        let inner = self.lock_inner("surface lookup")?;
        let surface_id = surface.id();
        let keyboard_id = inner.surface_to_keyboard.get(&surface_id)?;
        let seat_id = inner.keyboard_to_seat.get(&keyboard_id)?;
        inner.input_by_seat.get(&seat_id).cloned()
    }

    fn get_text_input_for_seat(
        &self,
        seat: &WlSeat,
        qh: &QueueHandle<WaylandState>,
    ) -> Option<ZwpTextInputV3> {
        let mgr = &self.text_input_manager;
        let mut inner = self.lock_inner("seat lookup")?;
        let seat_id = seat.id();
        let input = inner.input_by_seat.entry(seat_id).or_insert_with(|| {
            let input = mgr.get_text_input(seat, &qh, TextInputData::default());
            input.into()
        });
        Some(input.clone())
    }

    pub(super) fn advise_surface(&self, surface: &WlSurface, keyboard: &WlKeyboard) {
        let surface_id = surface.id();
        let keyboard_id = keyboard.id();
        if let Some(mut inner) = self.lock_inner("surface advice") {
            inner.surface_to_keyboard.insert(surface_id, keyboard_id);
        }
    }

    pub(super) fn advise_seat(
        &self,
        seat: &WlSeat,
        keyboard: &WlKeyboard,
        qh: &QueueHandle<WaylandState>,
    ) {
        self.get_text_input_for_seat(seat, qh);
        let keyboard_id = keyboard.id();
        let seat_id = seat.id();
        if let Some(mut inner) = self.lock_inner("seat advice") {
            inner.keyboard_to_seat.insert(keyboard_id, seat_id);
        }
    }

    pub(super) fn forget_keyboard(&self, keyboard: &WlKeyboard) {
        let keyboard_id = keyboard.id();
        let Some(mut inner) = self.lock_inner("keyboard removal") else {
            return;
        };
        inner.keyboard_to_seat.remove(&keyboard_id);
        inner
            .surface_to_keyboard
            .retain(|_, mapped_keyboard| mapped_keyboard != &keyboard_id);
    }

    pub(super) fn forget_seat(&self, seat: &WlSeat) {
        let seat_id = seat.id();
        let Some(mut inner) = self.lock_inner("seat removal") else {
            return;
        };

        inner
            .keyboard_to_seat
            .retain(|_, mapped_seat| mapped_seat != &seat_id);
        let live_keyboards: HashSet<_> = inner.keyboard_to_seat.keys().cloned().collect();
        inner
            .surface_to_keyboard
            .retain(|_, keyboard_id| live_keyboards.contains(keyboard_id));

        if let Some(input) = inner.input_by_seat.remove(&seat_id) {
            input.disable();
            input.commit();
            inner.pending_state.remove(&input.id());
        }
    }

    /// Workaround for <https://gitlab.gnome.org/GNOME/gnome-shell/-/issues/4776>
    /// If we make sure to disable things before we close the app,
    /// mutter is less likely to get in a bad state
    pub fn shutdown(&self) {
        if let Some(mut inner) = self.lock_inner("shutdown") {
            inner.disable_all();
        }
    }
}

impl Inner {
    fn disable_all(&mut self) {
        for input in self.input_by_seat.values() {
            input.disable();
            input.commit();
        }
    }
}

#[derive(Default)]
pub(super) struct TextInputData {
    // XXX: inner could probably be moved here
    _inner: Mutex<TextInputDataInner>,
}

#[derive(Default)]
pub(super) struct TextInputDataInner {}

impl Dispatch<ZwpTextInputManagerV3, GlobalData, WaylandState> for TextInputState {
    fn event(
        _state: &mut WaylandState,
        _proxy: &ZwpTextInputManagerV3,
        _event: <ZwpTextInputManagerV3 as Proxy>::Event,
        _data: &GlobalData,
        _conn: &wayland_client::Connection,
        _qhandle: &QueueHandle<WaylandState>,
    ) {
        // No events from ZwpTextInputMangerV3
        unreachable!();
    }
}

impl Dispatch<ZwpTextInputV3, TextInputData, WaylandState> for TextInputState {
    fn event(
        state: &mut WaylandState,
        input: &ZwpTextInputV3,
        event: <ZwpTextInputV3 as Proxy>::Event,
        _data: &TextInputData,
        _conn: &wayland_client::Connection,
        _qhandle: &QueueHandle<WaylandState>,
    ) {
        log::trace!("ZwpTextInputEvent: {event:?}");
        let mut pending_state = {
            let Some(text_input) = state.text_input.as_ref() else {
                log::warn!("Wayland text input event arrived without text input state");
                return;
            };
            let Some(mut inner) = text_input.lock_inner("text input event") else {
                return;
            };
            inner.pending_state.entry(input.id()).or_default().clone()
        };

        match event {
            TextInputEvent::PreeditString {
                text,
                cursor_begin: _,
                cursor_end: _,
            } => {
                pending_state.pre_edit = text;
            }
            TextInputEvent::CommitString { text } => {
                pending_state.commit = text;
                state.dispatch_to_focused_window(WindowEvent::AdviseDeadKeyStatus(
                    DeadKeyStatus::None,
                ));
            }
            TextInputEvent::Done { serial } => {
                *state.last_serial.borrow_mut() = serial;
                if let Some(text) = pending_state.commit.take() {
                    state.dispatch_to_focused_window(WindowEvent::KeyEvent(KeyEvent {
                        key: KeyCode::composed(&text),
                        modifiers: Modifiers::NONE,
                        leds: KeyboardLedStatus::empty(),
                        repeat_count: 1,
                        key_is_down: true,
                        raw: None,
                    }));
                }
                let status = if let Some(text) = pending_state.pre_edit.take() {
                    DeadKeyStatus::Composing(text)
                } else {
                    DeadKeyStatus::None
                };
                state.dispatch_to_focused_window(WindowEvent::AdviseDeadKeyStatus(status));
            }
            _ => {}
        }

        let Some(text_input) = state.text_input.as_ref() else {
            log::warn!("Wayland text input state disappeared before event completion");
            return;
        };
        if let Some(mut inner) = text_input.lock_inner("text input event completion") {
            inner.pending_state.insert(input.id(), pending_state);
        }
    }
}

impl WaylandState {
    fn dispatch_to_focused_window(&self, event: WindowEvent) {
        if let Some(&window_id) = self.keyboard_window_id.borrow().as_ref() {
            if let Some(win) = self.window_by_id(window_id) {
                let mut inner = win.borrow_mut();
                inner.events.dispatch(event);
            }
        }
    }
}

impl Drop for WaylandState {
    fn drop(&mut self) {
        if let Some(text_input) = self.text_input.as_mut() {
            text_input.shutdown();
        }
    }
}
