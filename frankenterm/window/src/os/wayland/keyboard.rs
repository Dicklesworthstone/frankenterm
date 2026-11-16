use std::collections::BTreeSet;

use wayland_client::protocol::wl_keyboard::{
    Event as WlKeyboardEvent, KeyState, KeymapFormat, WlKeyboard,
};
use wayland_client::{Dispatch, Proxy, WEnum};
use wezterm_input_types::KeyboardLedStatus;
use xkbcommon::xkb;
use xkbcommon::xkb::CONTEXT_NO_FLAGS;

use crate::x11::KeyboardWithFallback;
use crate::{Handled, KeyCode, Modifiers, RawKeyEvent, WindowEvent, WindowEventSender};

use super::state::{clear_surface_authority_if_matches, replace_surface_authority, WaylandState};
use super::SurfaceUserData;

/// A missing or rejected keymap is different from the compositor explicitly
/// selecting raw keycodes. Never retain an XKB mapper across either transition.
#[derive(Default)]
pub(super) enum KeyboardTranslation {
    #[default]
    Unavailable,
    Xkb(Box<KeyboardWithFallback>),
    RawNoKeymap(RawKeyboardState),
}

impl KeyboardTranslation {
    pub(super) fn mapper(&self) -> Option<&KeyboardWithFallback> {
        match self {
            Self::Xkb(mapper) => Some(mapper),
            Self::Unavailable | Self::RawNoKeymap(_) => None,
        }
    }

    pub(super) fn mapper_mut(&mut self) -> Option<&mut KeyboardWithFallback> {
        match self {
            Self::Xkb(mapper) => Some(mapper),
            Self::Unavailable | Self::RawNoKeymap(_) => None,
        }
    }

    pub(super) fn retire(&mut self) {
        *self = Self::Unavailable;
    }

    fn begin_keymap(&mut self, format: WEnum<KeymapFormat>) -> Option<KeymapFormat> {
        self.retire();
        match format.into_result() {
            Ok(KeymapFormat::XkbV1) => Some(KeymapFormat::XkbV1),
            Ok(KeymapFormat::NoKeymap) => {
                *self = Self::RawNoKeymap(RawKeyboardState::default());
                Some(KeymapFormat::NoKeymap)
            }
            Ok(_) | Err(_) => {
                log::warn!("Wayland keyboard disabled: unsupported keymap format");
                None
            }
        }
    }

    fn install_xkb(&mut self, source: String) -> anyhow::Result<()> {
        self.retire();
        let mapper = KeyboardWithFallback::new_from_string(source)?;
        *self = Self::Xkb(Box::new(mapper));
        Ok(())
    }

    pub(super) fn clear_raw_focus(&mut self) {
        if let Self::RawNoKeymap(raw) = self {
            raw.clear_focus();
        }
    }

    pub(super) fn update_modifiers(
        &mut self,
        depressed: u32,
        latched: u32,
        locked: u32,
        group: u32,
    ) {
        match self {
            Self::Xkb(mapper) => mapper.update_modifier_state(depressed, latched, locked, group),
            Self::RawNoKeymap(raw) => {
                raw.modifiers = Some([depressed, latched, locked, group]);
            }
            Self::Unavailable => {}
        }
    }
}

// Bound compositor-owned pressed-key metadata independently of keycode values.
const MAX_RAW_PRESSED_KEYS: usize = 1024;

/// Raw mode supports explicit `raw:N` bindings only. Modifier bit meanings,
/// physical locations, repeatability and text all require a keymap. Until a
/// fresh modifier event establishes an empty state, even an unmodified binding
/// would be an unsupported interpretation of the compositor's input.
#[derive(Default)]
pub(super) struct RawKeyboardState {
    focused: bool,
    modifiers: Option<[u32; 4]>,
    pressed: BTreeSet<u32>,
    refusal_reported: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RawInputRejection {
    Unfocused,
    ModifiersUnavailable,
    ModifiersUnsupported,
    KeyStateUnsupported,
    KeyNotPressed,
    PressedKeyLimit,
    InvalidEnterKeys,
}

impl RawKeyboardState {
    fn clear_focus(&mut self) {
        self.focused = false;
        self.modifiers = None;
        self.pressed.clear();
    }

    pub(super) fn enter(&mut self, keys: &[u8]) {
        self.clear_focus();
        let (keys, remainder) = keys.as_chunks::<4>();
        if !remainder.is_empty() || keys.len() > MAX_RAW_PRESSED_KEYS {
            self.report_refusal(RawInputRejection::InvalidEnterKeys);
            return;
        }
        self.focused = true;
        self.pressed
            .extend(keys.iter().map(|key| u32::from_ne_bytes(*key)));
    }

    fn key_event(
        &mut self,
        key: u32,
        state: WEnum<KeyState>,
    ) -> Result<RawKeyEvent, RawInputRejection> {
        if !self.focused {
            return Err(RawInputRejection::Unfocused);
        }
        let pressed = match state.into_result() {
            Ok(KeyState::Pressed) => {
                if !self.pressed.contains(&key) && self.pressed.len() >= MAX_RAW_PRESSED_KEYS {
                    return Err(RawInputRejection::PressedKeyLimit);
                }
                self.pressed.insert(key);
                true
            }
            Ok(KeyState::Repeated) if self.pressed.contains(&key) => true,
            Ok(KeyState::Released) if self.pressed.remove(&key) => false,
            Ok(KeyState::Repeated | KeyState::Released) => {
                return Err(RawInputRejection::KeyNotPressed);
            }
            Ok(_) | Err(_) => return Err(RawInputRejection::KeyStateUnsupported),
        };

        match self.modifiers {
            Some([0, 0, 0, 0]) => {}
            Some(_) => return Err(RawInputRejection::ModifiersUnsupported),
            None => return Err(RawInputRejection::ModifiersUnavailable),
        }

        // The protocol's +8 conversion is specific to XkbV1. NoKeymap already
        // supplies the raw u32 keycode; preserving it exactly cannot overflow.
        Ok(RawKeyEvent {
            key: KeyCode::RawCode(key),
            raw_code: key,
            phys_code: None,
            modifiers: Modifiers::NONE,
            leds: KeyboardLedStatus::empty(),
            repeat_count: 1,
            key_is_down: pressed,
            handled: Handled::new(),
        })
    }

    pub(super) fn dispatch_key(
        &mut self,
        key: u32,
        state: WEnum<KeyState>,
        events: &mut WindowEventSender,
    ) {
        match self.key_event(key, state) {
            Ok(event) => events.dispatch(WindowEvent::RawKeyEvent(event)),
            Err(reason) => self.report_refusal(reason),
        }
    }

    fn report_refusal(&mut self, reason: RawInputRejection) {
        if !self.refusal_reported {
            self.refusal_reported = true;
            log::warn!("Wayland raw keyboard binding unavailable: reason={reason:?}");
        }
    }
}

fn cancel_window_key_repeat(state: &WaylandState, window_id: usize) {
    if let Some(window) = state.window_by_id(window_id) {
        window.borrow_mut().cancel_key_repeat();
    }
}

fn disable_text_input_for_keyboard(state: &WaylandState, keyboard: &WlKeyboard) {
    state.disable_keyboard_text_input(keyboard);
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
    focused_window.map_or(
        ModifierRoute::UpdateMapperOnly,
        ModifierRoute::DispatchToWindow,
    )
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
        if state
            .keyboard
            .as_ref()
            .is_none_or(|current| current.id() != keyboard.id())
        {
            return;
        }
        let mut event_window_id = None;
        match &event {
            WlKeyboardEvent::Enter {
                serial, surface, ..
            } => {
                *state.last_serial.borrow_mut() = *serial;
                let entered_window_id =
                    SurfaceUserData::try_from_wl(surface).map(|data| data.window_id);
                {
                    let mut active_surface = state.keyboard_active_surface_id.borrow_mut();
                    replace_surface_authority(&mut *active_surface, surface.id());
                }
                let route = route_keyboard_enter(state.keyboard_window_id, entered_window_id);
                if let Some(window_id) = route.cancel_repeat_for {
                    let previous_window = state.window_by_id(window_id);
                    if let Some(previous_window) = previous_window {
                        // Enter should normally be preceded by Leave.  If it
                        // is not, retire the old focus coherently before the
                        // new window receives Enter(true). Preserve the
                        // seat-global mapper when focus transfers directly to
                        // another known surface; the new window must inherit
                        // the compositor's current modifier state.
                        if entered_window_id.is_some() {
                            previous_window.borrow_mut().emit_focus_transfer_out();
                        } else {
                            previous_window
                                .borrow_mut()
                                .emit_focus(state.keyboard_translation.mapper_mut(), false);
                        }
                    } else {
                        log::warn!(
                            "Wayland keyboard focus transfer referenced missing window {window_id}"
                        );
                    }
                }
                state.keyboard_window_id = route.focused;
                state.keyboard_translation.clear_raw_focus();
                event_window_id = route.dispatch_to;
                if entered_window_id.is_some() {
                    // Protocol text-input Enter may precede or follow this
                    // event. Enable only when both authorities name this surface.
                    state.advise_keyboard_text_input(surface, keyboard);
                } else {
                    disable_text_input_for_keyboard(state, keyboard);
                    log::warn!("Ignoring Wayland keyboard Enter without a known window");
                }
            }
            WlKeyboardEvent::Leave {
                serial, surface, ..
            } => {
                *state.last_serial.borrow_mut() = *serial;
                let left_surface_id = surface.id();
                let cleared = {
                    let mut active_surface = state.keyboard_active_surface_id.borrow_mut();
                    clear_surface_authority_if_matches(&mut *active_surface, &left_surface_id)
                };
                if !cleared {
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
                state.keyboard_translation.clear_raw_focus();
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
                // Modifier state belongs to the wl_keyboard/seat, not to a
                // particular window. Raw mode retains protocol state without
                // guessing the names of any nonzero modifier bits.
                state.keyboard_translation.update_modifiers(
                    *mods_depressed,
                    *mods_latched,
                    *mods_locked,
                    *group,
                );
                match route_modifiers(state.keyboard_window_id) {
                    ModifierRoute::DispatchToWindow(window_id) => {
                        event_window_id = Some(window_id);
                    }
                    ModifierRoute::UpdateMapperOnly => {
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
                    if let Some(window) = state.window_by_id(window_id) {
                        window.borrow_mut().retire_keyboard_mapping();
                    }
                }
                let Some(format) = state.keyboard_translation.begin_keymap(*format) else {
                    return;
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
                                if let Err(err) = state.keyboard_translation.install_xkb(s) {
                                    log::error!("Error processing keymap change: {:#}", err);
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
                        if state
                            .keyboard_window_id
                            .and_then(|window_id| state.window_by_id(window_id))
                            .is_some()
                        {
                            if let KeyboardTranslation::RawNoKeymap(raw) =
                                &mut state.keyboard_translation
                            {
                                // A mode change can occur without another
                                // Enter. Retain focus, but not keys or modifier
                                // knowledge derived under the old keymap.
                                raw.enter(&[]);
                            }
                        }
                        log::debug!(
                            "Wayland keyboard uses raw-code bindings without text translation"
                        );
                    }
                    _ => {
                        log::warn!("Ignoring unsupported Wayland keymap format {format:?}");
                    }
                }
            }
            _ => {
                log::trace!("Ignoring unsupported Wayland keyboard event");
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
            &mut state.keyboard_translation,
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
        KeyboardTranslation, ModifierRoute, RawInputRejection, RawKeyboardState,
        MAX_RAW_PRESSED_KEYS,
    };
    use crate::{KeyCode, Modifiers};
    use std::convert::TryFrom;
    use wayland_client::protocol::wl_keyboard::{KeyState, KeymapFormat};
    use wayland_client::WEnum;
    use xkbcommon::xkb;

    #[test]
    fn raw_no_keymap_preserves_wire_codes_without_xkb_offset_or_symbols() {
        let mut raw = RawKeyboardState {
            focused: true,
            modifiers: Some([0; 4]),
            ..Default::default()
        };
        for key in [0, 30, 248, u32::MAX - 8, u32::MAX] {
            for state in [KeyState::Pressed, KeyState::Repeated, KeyState::Released] {
                let event = raw.key_event(key, WEnum::Value(state)).unwrap();
                assert_eq!(event.key, KeyCode::RawCode(key));
                assert_eq!(event.raw_code, key);
                assert_eq!(event.phys_code, None);
                assert_eq!(event.modifiers, Modifiers::NONE);
                assert!(event.leds.is_empty());
                assert_eq!(event.repeat_count, 1);
                assert_eq!(event.key_is_down, state != KeyState::Released);
            }
            assert_eq!(
                raw.key_event(key, WEnum::Value(KeyState::Repeated)),
                Err(RawInputRejection::KeyNotPressed)
            );
        }
        assert!(raw.pressed.is_empty());
    }

    #[test]
    fn raw_no_keymap_never_guesses_unknown_or_nonzero_modifiers() {
        let mut translation = KeyboardTranslation::default();
        assert_eq!(
            translation.begin_keymap(WEnum::Value(KeymapFormat::NoKeymap)),
            Some(KeymapFormat::NoKeymap)
        );
        let KeyboardTranslation::RawNoKeymap(raw) = &mut translation else {
            panic!("explicit NoKeymap must enable raw mode");
        };
        raw.enter(&[]);
        assert_eq!(
            raw.key_event(30, WEnum::Value(KeyState::Pressed)),
            Err(RawInputRejection::ModifiersUnavailable)
        );

        for modifiers in [[1, 0, 0, 0], [0, 1, 0, 0], [0, 0, 1, 0], [0, 0, 0, 1]] {
            translation.update_modifiers(modifiers[0], modifiers[1], modifiers[2], modifiers[3]);
            let KeyboardTranslation::RawNoKeymap(raw) = &mut translation else {
                panic!("modifier updates must preserve raw mode");
            };
            assert_eq!(
                raw.key_event(30, WEnum::Value(KeyState::Repeated)),
                Err(RawInputRejection::ModifiersUnsupported)
            );
        }

        // These updates also run without a focused window. A later empty
        // protocol state permits raw bindings without interpreting bit names.
        translation.update_modifiers(0, 0, 0, 0);
        let KeyboardTranslation::RawNoKeymap(raw) = &mut translation else {
            panic!("empty modifier update must preserve raw mode");
        };
        assert!(raw.key_event(30, WEnum::Value(KeyState::Repeated)).is_ok());
    }

    #[test]
    fn raw_no_keymap_enter_and_focus_retirement_require_fresh_authority() {
        let mut raw = RawKeyboardState {
            focused: true,
            modifiers: Some([0; 4]),
            ..Default::default()
        };
        raw.key_event(30, WEnum::Value(KeyState::Pressed)).unwrap();
        raw.enter(&31_u32.to_ne_bytes());
        assert_eq!(raw.pressed.iter().copied().collect::<Vec<_>>(), vec![31]);
        assert_eq!(
            raw.key_event(31, WEnum::Value(KeyState::Repeated)),
            Err(RawInputRejection::ModifiersUnavailable)
        );
        raw.modifiers = Some([0; 4]);
        assert!(raw.key_event(31, WEnum::Value(KeyState::Repeated)).is_ok());

        let mut translation = KeyboardTranslation::RawNoKeymap(raw);
        translation.clear_raw_focus();
        let KeyboardTranslation::RawNoKeymap(raw) = &mut translation else {
            panic!("focus loss does not remove the protocol keymap format");
        };
        assert!(raw.pressed.is_empty());
        assert!(raw.modifiers.is_none());
        raw.modifiers = Some([0; 4]);
        assert_eq!(
            raw.key_event(31, WEnum::Value(KeyState::Repeated)),
            Err(RawInputRejection::Unfocused)
        );
        assert_eq!(
            raw.key_event(32, WEnum::Value(KeyState::Pressed)),
            Err(RawInputRejection::Unfocused)
        );
        assert!(raw.pressed.is_empty());
        raw.enter(&[]);
        raw.modifiers = Some([0; 4]);
        assert_eq!(
            raw.key_event(31, WEnum::Value(KeyState::Repeated)),
            Err(RawInputRejection::KeyNotPressed)
        );
        assert!(raw.key_event(32, WEnum::Value(KeyState::Pressed)).is_ok());
        translation.retire();
        assert!(matches!(translation, KeyboardTranslation::Unavailable));
    }

    #[test]
    fn raw_no_keymap_unfocused_modifiers_cannot_authorize_binding_dispatch() {
        let mut translation = KeyboardTranslation::RawNoKeymap(RawKeyboardState::default());
        for modifiers in [1, 0] {
            translation.update_modifiers(modifiers, 0, 0, 0);
            let KeyboardTranslation::RawNoKeymap(raw) = &mut translation else {
                unreachable!();
            };
            assert_eq!(
                raw.key_event(30, WEnum::Value(KeyState::Pressed)),
                Err(RawInputRejection::Unfocused)
            );
            assert!(raw.pressed.is_empty());
        }

        let KeyboardTranslation::RawNoKeymap(raw) = &mut translation else {
            unreachable!();
        };
        raw.enter(&30_u32.to_ne_bytes());
        assert_eq!(
            raw.key_event(30, WEnum::Value(KeyState::Repeated)),
            Err(RawInputRejection::ModifiersUnavailable)
        );
        translation.update_modifiers(0, 0, 0, 0);
        let KeyboardTranslation::RawNoKeymap(raw) = &mut translation else {
            unreachable!();
        };
        assert!(raw.key_event(30, WEnum::Value(KeyState::Repeated)).is_ok());
    }

    #[test]
    fn raw_no_keymap_bounds_pressed_metadata_and_rejects_malformed_enter() {
        let mut raw = RawKeyboardState {
            focused: true,
            modifiers: Some([0; 4]),
            ..Default::default()
        };
        let limit = u32::try_from(MAX_RAW_PRESSED_KEYS).unwrap();
        for key in 0..limit {
            raw.key_event(key, WEnum::Value(KeyState::Pressed)).unwrap();
        }
        assert_eq!(raw.pressed.len(), MAX_RAW_PRESSED_KEYS);
        assert_eq!(
            raw.key_event(limit, WEnum::Value(KeyState::Pressed)),
            Err(RawInputRejection::PressedKeyLimit)
        );
        raw.key_event(0, WEnum::Value(KeyState::Released)).unwrap();
        raw.key_event(limit, WEnum::Value(KeyState::Pressed))
            .unwrap();
        assert_eq!(raw.pressed.len(), MAX_RAW_PRESSED_KEYS);

        raw.enter(&[1, 2, 3]);
        assert!(raw.pressed.is_empty());
        assert!(raw.modifiers.is_none());
        raw.enter(&vec![0; (MAX_RAW_PRESSED_KEYS + 1) * 4]);
        assert!(raw.pressed.is_empty());
        assert!(raw.modifiers.is_none());
        raw.enter(&[]);
        assert_eq!(
            raw.key_event(0, WEnum::Unknown(u32::MAX)),
            Err(RawInputRejection::KeyStateUnsupported)
        );
        assert!(raw.pressed.is_empty());
    }

    #[test]
    fn raw_no_keymap_replacement_never_reuses_an_old_xkb_mapper() -> anyhow::Result<()> {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_names(
            &context,
            "",
            "",
            "us",
            "",
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .ok_or_else(|| anyhow::anyhow!("failed to construct test keymap"))?;
        let source = keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
        let mut translation = KeyboardTranslation::default();
        translation.install_xkb(source.clone())?;
        assert!(translation.mapper().is_some());

        assert_eq!(
            translation.begin_keymap(WEnum::Value(KeymapFormat::NoKeymap)),
            Some(KeymapFormat::NoKeymap)
        );
        assert!(translation.mapper().is_none());
        let KeyboardTranslation::RawNoKeymap(raw) = &translation else {
            panic!("NoKeymap must select raw mode");
        };
        assert!(raw.modifiers.is_none());

        assert_eq!(
            translation.begin_keymap(WEnum::Value(KeymapFormat::XkbV1)),
            Some(KeymapFormat::XkbV1)
        );
        assert!(matches!(translation, KeyboardTranslation::Unavailable));
        assert!(translation
            .install_xkb("invalid keymap".to_string())
            .is_err());
        assert!(matches!(translation, KeyboardTranslation::Unavailable));
        translation.install_xkb(source)?;
        assert!(translation.mapper().is_some());

        assert!(translation.begin_keymap(WEnum::Unknown(u32::MAX)).is_none());
        assert!(matches!(translation, KeyboardTranslation::Unavailable));
        assert_eq!(
            translation.begin_keymap(WEnum::Value(KeymapFormat::NoKeymap)),
            Some(KeymapFormat::NoKeymap)
        );
        translation.update_modifiers(0, 0, 0, 0);
        assert_eq!(
            translation.begin_keymap(WEnum::Value(KeymapFormat::NoKeymap)),
            Some(KeymapFormat::NoKeymap)
        );
        let KeyboardTranslation::RawNoKeymap(raw) = &translation else {
            panic!("replacement NoKeymap must select fresh raw mode");
        };
        assert!(raw.modifiers.is_none());
        assert!(raw.pressed.is_empty());
        Ok(())
    }

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
        assert_eq!(route_modifiers(Some(9)), ModifierRoute::DispatchToWindow(9));
    }
}
