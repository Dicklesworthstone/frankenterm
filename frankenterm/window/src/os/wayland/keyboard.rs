use wayland_client::protocol::wl_keyboard::{Event as WlKeyboardEvent, KeymapFormat, WlKeyboard};
use wayland_client::{Dispatch, Proxy};
use xkbcommon::xkb;
use xkbcommon::xkb::CONTEXT_NO_FLAGS;

use crate::x11::KeyboardWithFallback;

use super::state::{
    clear_surface_authority_if_matches, replace_surface_authority, WaylandState,
};
use super::SurfaceUserData;

fn cancel_window_key_repeat(state: &WaylandState, window_id: usize) {
    if let Some(window) = state.window_by_id(window_id) {
        window.borrow_mut().cancel_key_repeat();
    }
}

fn disable_text_input_for_keyboard(state: &WaylandState, keyboard: &WlKeyboard) {
    if let Some(text_input) = &state.text_input {
        if let Some(input) = text_input.get_text_input_for_keyboard(keyboard) {
            input.disable();
            input.commit();
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct KeyboardFocusRoute {
    dispatch_to: Option<usize>,
    focused: Option<usize>,
    cancel_repeat_for: Option<usize>,
}

fn route_keyboard_enter(
    currently_focused: Option<usize>,
    entered: Option<usize>,
) -> KeyboardFocusRoute {
    let cancel_repeat_for = if currently_focused != entered {
        currently_focused
    } else {
        None
    };
    KeyboardFocusRoute {
        dispatch_to: entered,
        focused: entered,
        cancel_repeat_for,
    }
}

fn route_keyboard_leave(
    currently_focused: Option<usize>,
    left: Option<usize>,
) -> KeyboardFocusRoute {
    // The caller has already established that this Leave names the active
    // keyboard surface.  Prefer the current window identity if surface user
    // data disappeared during destruction.
    let dispatch_to = currently_focused.or(left);
    KeyboardFocusRoute {
        dispatch_to,
        focused: None,
        cancel_repeat_for: dispatch_to,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ModifierRoute {
    UpdateMapperOnly,
    DispatchToWindow(usize),
}

fn route_modifiers(focused_window: Option<usize>) -> ModifierRoute {
    focused_window.map_or(ModifierRoute::UpdateMapperOnly, ModifierRoute::DispatchToWindow)
}

fn update_unfocused_modifier_state(
    mapper: Option<&mut KeyboardWithFallback>,
    mods_depressed: u32,
    mods_latched: u32,
    mods_locked: u32,
    group: u32,
) {
    if let Some(mapper) = mapper {
        mapper.update_modifier_state(mods_depressed, mods_latched, mods_locked, group);
    }
}

// We can't use the xkbcommon feature because it is too abstract for us
impl Dispatch<WlKeyboard, KeyboardData> for WaylandState {
    fn event(
        state: &mut WaylandState,
        keyboard: &WlKeyboard,
        event: <WlKeyboard as wayland_client::Proxy>::Event,
        _data: &KeyboardData,
        _conn: &wayland_client::Connection,
        _qhandle: &wayland_client::QueueHandle<WaylandState>,
    ) {
        log::trace!("We reached an event here: {:?}???", event);
        let mut event_window_id = None;
        match &event {
            WlKeyboardEvent::Enter {
                serial, surface, ..
            } => {
                *state.last_serial.borrow_mut() = *serial;
                let entered_window_id =
                    SurfaceUserData::try_from_wl(surface).map(|data| data.window_id);
                replace_surface_authority(
                    &mut state.keyboard_active_surface_id.borrow_mut(),
                    surface.id(),
                );
                let route = route_keyboard_enter(state.keyboard_window_id, entered_window_id);
                if let Some(window_id) = route.cancel_repeat_for {
                    cancel_window_key_repeat(state, window_id);
                    let previous_window = state.window_by_id(window_id);
                    if let Some(previous_window) = previous_window {
                        // Enter should normally be preceded by Leave.  If it
                        // is not, retire the old focus coherently before the
                        // new window receives Enter(true).
                        previous_window
                            .borrow_mut()
                            .emit_focus(state.keyboard_mapper.as_mut(), false);
                    }
                }
                state.keyboard_window_id = route.focused;
                event_window_id = route.dispatch_to;
                if entered_window_id.is_some() {
                    if let Some(text_input) = &state.text_input {
                        if let Some(input) = text_input.get_text_input_for_keyboard(keyboard) {
                            input.enable();
                            input.commit();
                        }
                        text_input.advise_surface(surface, keyboard);
                    }
                } else {
                    disable_text_input_for_keyboard(state, keyboard);
                    log::warn!("{:?}, no known surface", event);
                }
            }
            WlKeyboardEvent::Leave {
                serial, surface, ..
            } => {
                *state.last_serial.borrow_mut() = *serial;
                let left_surface_id = surface.id();
                if !clear_surface_authority_if_matches(
                    &mut state.keyboard_active_surface_id.borrow_mut(),
                    &left_surface_id,
                ) {
                    log::warn!(
                        "Ignoring stale Wayland keyboard Leave for surface {left_surface_id:?}"
                    );
                    return;
                }
                let surface_window_id =
                    SurfaceUserData::try_from_wl(surface).map(|data| data.window_id);
                if let (Some(current), Some(left)) = (state.keyboard_window_id, surface_window_id) {
                    if current != left {
                        log::warn!(
                            "Wayland keyboard Leave for stale window {left}; clearing current window {current}"
                        );
                    }
                }
                let route = route_keyboard_leave(state.keyboard_window_id, surface_window_id);
                if let Some(window_id) = route.cancel_repeat_for {
                    cancel_window_key_repeat(state, window_id);
                }
                state.keyboard_window_id = route.focused;
                event_window_id = route.dispatch_to;
                disable_text_input_for_keyboard(state, keyboard);
            }
            WlKeyboardEvent::Key { serial, .. } => {
                *state.last_serial.borrow_mut() = *serial;
                event_window_id = state.keyboard_window_id;
            }
            WlKeyboardEvent::Modifiers {
                serial,
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
            } => {
                *state.last_serial.borrow_mut() = *serial;
                match route_modifiers(state.keyboard_window_id) {
                    ModifierRoute::DispatchToWindow(window_id) => {
                        event_window_id = Some(window_id);
                    }
                    ModifierRoute::UpdateMapperOnly => {
                        update_unfocused_modifier_state(
                            state.keyboard_mapper.as_mut(),
                            *mods_depressed,
                            *mods_latched,
                            *mods_locked,
                            *group,
                        );
                        return;
                    }
                }
            }
            WlKeyboardEvent::RepeatInfo { rate, delay } => {
                state.key_repeat_rate = *rate;
                state.key_repeat_delay = *delay;
                event_window_id = state.keyboard_window_id;
            }
            WlKeyboardEvent::Keymap { format, fd, size } => {
                // A held repeat event and the current mapper were derived from
                // the preceding keymap. Retire both before attempting to
                // install the replacement so NoKeymap, unknown formats, and
                // malformed replacement data fail closed rather than decoding
                // subsequent keys through a stale layout.
                if let Some(window_id) = state.keyboard_window_id {
                    cancel_window_key_repeat(state, window_id);
                }
                state.keyboard_mapper.take();
                let format = match format.into_result() {
                    Ok(format) => format,
                    Err(raw_format) => {
                        log::warn!("Ignoring Wayland keymap with unknown format: {raw_format:?}");
                        return;
                    }
                };

                match format {
                    KeymapFormat::XkbV1 => {
                        // In later protocol versions, the fd must be privately mmap'd.
                        // We let xkb handle this and then turn it back into a string.
                        #[allow(unused_unsafe)] // Upstream release will change this
                        match unsafe {
                            let context = xkb::Context::new(CONTEXT_NO_FLAGS);
                            let cloned_fd = match fd.try_clone() {
                                Ok(fd) => fd,
                                Err(err) => {
                                    log::error!("Could not clone Wayland keymap fd: {err:#}");
                                    return;
                                }
                            };
                            xkb::Keymap::new_from_fd(
                                &context,
                                cloned_fd,
                                *size as usize,
                                xkb::KEYMAP_FORMAT_TEXT_V1,
                                xkb::COMPILE_NO_FLAGS,
                            )
                        } {
                            Ok(Some(keymap)) => {
                                let s = keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
                                match KeyboardWithFallback::new_from_string(s) {
                                    Ok(k) => {
                                        state.keyboard_mapper.replace(k);
                                    }
                                    Err(err) => {
                                        log::error!("Error processing keymap change: {:#}", err);
                                    }
                                }
                            }
                            Ok(None) => {
                                log::error!("invalid keymap");
                            }

                            Err(err) => {
                                log::error!("Error processing keymap change: {:#}", err);
                            }
                        }
                    }
                    KeymapFormat::NoKeymap => {
                        log::debug!(
                            "Wayland compositor removed the XKB keymap; mapped key input remains disabled"
                        );
                    }
                    _ => {
                        log::warn!("Ignoring unsupported Wayland keymap format {format:?}");
                    }
                }
            }
            _ => {
                log::trace!("Ignoring unhandled wl_keyboard event: {:?}", event);
            }
        }

        let Some(window_id) = event_window_id else {
            return;
        };
        let Some(win) = state.window_by_id(window_id) else {
            return;
        };
        // The dispatch loop already holds the sole mutable borrow of the
        // connection's WaylandState. Carry repeat_info forward from that
        // borrow rather than having the repeat scheduler re-borrow the global
        // state synchronously from inside keyboard_event.
        let key_repeat_rate = state.key_repeat_rate;
        let key_repeat_delay = state.key_repeat_delay;
        let mut inner = win.as_ref().borrow_mut();
        inner.keyboard_event(
            state.keyboard_mapper.as_mut(),
            event,
            window_id,
            key_repeat_rate,
            key_repeat_delay,
        );
    }
}

pub(super) struct KeyboardData {}

#[cfg(test)]
mod tests {
    use super::{
        route_keyboard_enter, route_keyboard_leave, route_modifiers, KeyboardFocusRoute,
        ModifierRoute,
    };

    #[test]
    fn leave_routes_to_current_window_before_clearing_focus() {
        assert_eq!(
            route_keyboard_leave(Some(7), Some(7)),
            KeyboardFocusRoute {
                dispatch_to: Some(7),
                focused: None,
                cancel_repeat_for: Some(7),
            }
        );
        assert_eq!(
            route_keyboard_leave(Some(7), None),
            KeyboardFocusRoute {
                dispatch_to: Some(7),
                focused: None,
                cancel_repeat_for: Some(7),
            }
        );
    }

    #[test]
    fn mismatched_leave_fails_closed_on_the_current_focus() {
        assert_eq!(
            route_keyboard_leave(Some(8), Some(7)),
            KeyboardFocusRoute {
                dispatch_to: Some(8),
                focused: None,
                cancel_repeat_for: Some(8),
            }
        );
    }

    #[test]
    fn enter_replaces_or_clears_the_previous_repeat_owner() {
        assert_eq!(
            route_keyboard_enter(Some(7), Some(8)),
            KeyboardFocusRoute {
                dispatch_to: Some(8),
                focused: Some(8),
                cancel_repeat_for: Some(7),
            }
        );
        assert_eq!(
            route_keyboard_enter(Some(7), None),
            KeyboardFocusRoute {
                dispatch_to: None,
                focused: None,
                cancel_repeat_for: Some(7),
            }
        );
    }

    #[test]
    fn modifiers_without_surface_focus_still_update_the_global_mapper() {
        assert_eq!(route_modifiers(None), ModifierRoute::UpdateMapperOnly);
        assert_eq!(
            route_modifiers(Some(9)),
            ModifierRoute::DispatchToWindow(9)
        );
    }
}
