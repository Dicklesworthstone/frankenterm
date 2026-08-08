use smithay_client_toolkit::seat::pointer::ThemeSpec;
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{Connection, Proxy, QueueHandle};

use crate::wayland::keyboard::KeyboardData;
use crate::wayland::pointer::PointerUserData;
use crate::wayland::SurfaceUserData;

use super::state::WaylandState;

impl SeatHandler for WaylandState {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat
    }

    fn new_seat(&mut self, _conn: &Connection, qh: &QueueHandle<Self>, seat: WlSeat) {
        log::trace!("Discovered Wayland seat {:?}", seat.id());
        self.ensure_selection_devices_for_seat(qh, &seat);
    }

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: WlSeat,
        capability: Capability,
    ) {
        self.ensure_selection_devices_for_seat(qh, &seat);

        match capability {
            Capability::Keyboard if self.keyboard.is_none() => {
                log::trace!("Setting keyboard capability");
                self.emit_keyboard_focus_lost();
                let keyboard = seat.get_keyboard(qh, KeyboardData {});
                self.seat_bindings.note_keyboard(seat.id());
                self.keyboard = Some(keyboard.clone());

                if let Some(text_input) = &self.text_input {
                    text_input.advise_seat(&seat, &keyboard, qh);
                }
            }
            Capability::Pointer if self.pointer.is_none() => {
                log::trace!("Setting pointer capability");
                self.clear_pointer_focus();
                let surface = self.compositor.create_surface(qh);
                let pointer = match self
                    .seat
                    .get_pointer_with_theme_and_data::<
                        WaylandState,
                        SurfaceUserData,
                        PointerUserData,
                    >(
                        qh,
                        &seat,
                        self.shm.wl_shm(),
                        surface,
                        ThemeSpec::System,
                        PointerUserData::new(seat.clone()),
                    ) {
                    Ok(pointer) => pointer,
                    Err(err) => {
                        log::warn!(
                            "Failed to create themed Wayland pointer for seat {:?}: {err:?}",
                            seat.id()
                        );
                        return;
                    }
                };
                self.seat_bindings.note_pointer(seat.id());
                self.pointer = Some(pointer);
            }
            Capability::Touch /* if self.touch.is_none() */ => {
                log::trace!("Ignoring unsupported touch capability for seat {:?}", seat.id());
            }
            _ => {}
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        seat: WlSeat,
        capability: Capability,
    ) {
        match capability {
            Capability::Keyboard => {
                if self.seat_bindings.clear_keyboard_if_matches(&seat.id()) {
                    log::trace!("Lost keyboard capability for seat {:?}", seat.id());
                    self.disable_current_keyboard_text_input();
                    self.emit_keyboard_focus_lost();
                    if let Some(keyboard) = self.keyboard.take() {
                        if let Some(text_input) = &self.text_input {
                            text_input.forget_keyboard(&keyboard);
                        }
                        keyboard.release();
                    }
                    self.keyboard_mapper.take();
                }
            }
            Capability::Pointer => {
                if self.seat_bindings.clear_pointer_if_matches(&seat.id()) {
                    log::trace!("Lost pointer capability for seat {:?}", seat.id());
                    self.clear_pointer_focus();
                    if let Some(pointer) = self.pointer.take() {
                        pointer.pointer().release();
                    }
                }
            }
            Capability::Touch => {
                log::trace!("Lost touch capability for seat {:?}", seat.id());
            }
            _ => {}
        }
    }

    fn remove_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, seat: WlSeat) {
        log::trace!("Removing Wayland seat {:?}", seat.id());
        let cleanup = self.seat_bindings.clear_removed_seat(&seat.id());

        if cleanup.keyboard {
            self.disable_current_keyboard_text_input();
            self.emit_keyboard_focus_lost();
            if let Some(keyboard) = self.keyboard.take() {
                if let Some(text_input) = &self.text_input {
                    text_input.forget_keyboard(&keyboard);
                }
                keyboard.release();
            }
            self.keyboard_mapper.take();
        }

        if cleanup.pointer {
            self.clear_pointer_focus();
            if let Some(pointer) = self.pointer.take() {
                pointer.pointer().release();
            }
        }

        if cleanup.data_device {
            self.data_device.take();
            self.copy_paste_source.take();
            self.clear_copy_and_paste_offers();
        }

        if cleanup.primary_selection {
            self.primary_selection_device.take();
            self.primary_selection_source.take();
        }

        if let Some(text_input) = &self.text_input {
            text_input.forget_seat(&seat);
        }
    }
}

impl WaylandState {
    fn disable_current_keyboard_text_input(&self) {
        if let (Some(text_input), Some(keyboard)) = (&self.text_input, &self.keyboard) {
            if let Some(input) = text_input.get_text_input_for_keyboard(keyboard) {
                input.disable();
                input.commit();
            }
        }
    }

    fn emit_keyboard_focus_lost(&mut self) {
        self.keyboard_active_surface_id.get_mut().take();
        let Some(window_id) = self.keyboard_window_id.take() else {
            return;
        };
        let Some(window) = self.window_by_id(window_id) else {
            return;
        };
        window
            .borrow_mut()
            .emit_focus(self.keyboard_mapper.as_mut(), false);
    }

    fn ensure_selection_devices_for_seat(&mut self, qh: &QueueHandle<Self>, seat: &WlSeat) {
        if self.data_device.is_none() {
            let data_device_manager = &self.data_device_manager_state;
            self.data_device = Some(data_device_manager.get_data_device(qh, seat));
            self.seat_bindings.note_data_device(seat.id());
        }

        if self.primary_selection_device.is_none() {
            if let Some(manager) = &self.primary_selection_manager {
                self.primary_selection_device = Some(manager.get_selection_device(qh, seat));
                self.seat_bindings.note_primary_selection(seat.id());
            }
        }
    }
}
