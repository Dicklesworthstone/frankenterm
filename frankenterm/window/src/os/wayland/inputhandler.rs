//! Implements zwp_text_input_v3 for handling IME.
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
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
use super::{SurfaceUserData, WaylandWindowInner};

#[derive(Clone, Debug, PartialEq, Eq)]
struct FocusTarget<S> {
    surface_id: S,
    window_id: usize,
}

/// An owned reset can outlive the WaylandState borrow that retired focus.
pub(super) struct TextInputWindowReset {
    window: Rc<RefCell<WaylandWindowInner>>,
    surface_id: ObjectId,
}

impl TextInputWindowReset {
    pub(super) fn apply(self) {
        let mut window = self.window.borrow_mut();
        if window.has_text_input_surface(&self.surface_id) {
            window.reset_text_input();
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FocusCohort<S> {
    target: FocusTarget<S>,
    generation: u64,
}

#[derive(Default, Debug)]
struct PendingState {
    pre_edit: Option<String>,
    commit: Option<String>,
}

#[derive(Debug)]
struct PendingBatch<S> {
    focus: FocusCohort<S>,
    state: PendingState,
}

#[derive(Debug)]
struct FocusChange<S> {
    previous: Option<FocusTarget<S>>,
    current: Option<FocusTarget<S>>,
    disable: bool,
}

/// Text-input focus is independent of wl_keyboard event ordering. Both must
/// name the same live surface before enable or text delivery is authorized.
/// Pending events belong to this exact agreement, never to whichever window
/// happens to have keyboard focus when Done eventually arrives.
#[derive(Debug)]
struct InputFocus<S> {
    protocol: Option<FocusTarget<S>>,
    keyboard: Option<FocusTarget<S>>,
    active: Option<FocusCohort<S>>,
    pending: Option<PendingBatch<S>>,
    generation: u64,
    exhausted: bool,
    preedit_visible: bool,
}

impl<S> Default for InputFocus<S> {
    fn default() -> Self {
        Self {
            protocol: None,
            keyboard: None,
            active: None,
            pending: None,
            generation: 0,
            exhausted: false,
            preedit_visible: false,
        }
    }
}

impl<S: Clone + Eq> InputFocus<S> {
    fn reconcile(&mut self, may_disable: bool) -> FocusChange<S> {
        let previous = self.active.take().map(|focus| focus.target);
        self.pending = None;
        self.preedit_visible = false;

        if let Some(generation) = self.generation.checked_add(1) {
            self.generation = generation;
        } else {
            self.exhausted = true;
            log::error!("Wayland text input focus generation exhausted; disabling text delivery");
        }

        if !self.exhausted {
            if let (Some(protocol), Some(keyboard)) = (&self.protocol, &self.keyboard) {
                if protocol == keyboard {
                    self.active = Some(FocusCohort {
                        target: protocol.clone(),
                        generation: self.generation,
                    });
                }
            }
        }

        FocusChange {
            disable: may_disable && previous.is_some(),
            previous,
            current: self.active.as_ref().map(|focus| focus.target.clone()),
        }
    }

    fn keyboard(&mut self, target: Option<FocusTarget<S>>) -> Option<FocusChange<S>> {
        if self.keyboard == target {
            return None;
        }
        self.keyboard = target;
        Some(self.reconcile(true))
    }

    fn enter(&mut self, target: Option<FocusTarget<S>>) -> FocusChange<S> {
        self.protocol = target;
        // Even a repeated Enter starts a fresh agreement and drops any partial
        // batch. It must not inherit composition from an earlier enable.
        self.reconcile(true)
    }

    fn leave(&mut self, surface_id: &S) -> Option<FocusChange<S>> {
        if self.protocol.as_ref().map(|target| &target.surface_id) != Some(surface_id) {
            return None;
        }
        self.protocol = None;
        // Leave already disables this object's surface. Requests are ignored
        // until another Enter, so do not send a disable for a later surface.
        Some(self.reconcile(false))
    }

    fn forget_surface(&mut self, surface_id: &S) -> Option<FocusChange<S>> {
        let protocol_matches =
            self.protocol.as_ref().map(|target| &target.surface_id) == Some(surface_id);
        let keyboard_matches =
            self.keyboard.as_ref().map(|target| &target.surface_id) == Some(surface_id);
        if !protocol_matches && !keyboard_matches {
            return None;
        }
        if protocol_matches {
            self.protocol = None;
        }
        if keyboard_matches {
            self.keyboard = None;
        }
        Some(self.reconcile(true))
    }

    fn pending_mut(&mut self) -> Option<&mut PendingState> {
        let focus = self.active.as_ref()?.clone();
        Some(
            &mut self
                .pending
                .get_or_insert_with(|| PendingBatch {
                    focus,
                    state: PendingState::default(),
                })
                .state,
        )
    }

    fn preedit(&mut self, text: Option<String>) -> bool {
        let Some(pending) = self.pending_mut() else {
            return false;
        };
        pending.pre_edit = text;
        true
    }

    fn commit(&mut self, text: Option<String>) -> bool {
        let Some(pending) = self.pending_mut() else {
            return false;
        };
        pending.commit = text;
        true
    }

    fn done(&mut self, _serial: u32) -> Option<PendingBatch<S>> {
        let focus = self.active.as_ref()?.clone();
        let batch = match self.pending.take() {
            Some(batch) if batch.focus == focus => batch,
            Some(_) => {
                log::debug!("Ignoring Wayland text input batch from an obsolete focus generation");
                return None;
            }
            None if self.preedit_visible => PendingBatch {
                focus,
                state: PendingState::default(),
            },
            None => return None,
        };
        self.preedit_visible = batch
            .state
            .pre_edit
            .as_ref()
            .is_some_and(|text| !text.is_empty());
        // Done serial counts client commit requests on this text-input object,
        // not wl_seat authorization. The protocol requires applying text even
        // when that count differs from the latest request. This path makes no
        // text-input state requests in response to Done.
        Some(batch)
    }
}

#[derive(Debug)]
struct SeatInput {
    input: ZwpTextInputV3,
    focus: InputFocus<ObjectId>,
}

impl SeatInput {
    fn apply_focus_change(&self, change: FocusChange<ObjectId>) -> Vec<FocusTarget<ObjectId>> {
        if change.disable {
            self.input.disable();
            self.input.commit();
        }
        if change.current.is_some() {
            self.input.enable();
            self.input.commit();
        }
        let mut reset = Vec::new();
        for target in change.previous.into_iter().chain(change.current) {
            if !reset.contains(&target) {
                reset.push(target);
            }
        }
        reset
    }

    fn destroy(self) -> Option<FocusTarget<ObjectId>> {
        self.input.disable();
        self.input.commit();
        self.input.destroy();
        self.focus.active.map(|focus| focus.target)
    }
}

pub(super) struct TextInputState {
    text_input_manager: ZwpTextInputManagerV3,
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    input_by_seat: HashMap<ObjectId, SeatInput>,
    keyboard_to_seat: HashMap<ObjectId, ObjectId>,
    shutdown: bool,
}

impl Inner {
    fn for_keyboard(&mut self, keyboard: &WlKeyboard) -> Option<&mut SeatInput> {
        let seat_id = self.keyboard_to_seat.get(&keyboard.id())?;
        self.input_by_seat.get_mut(seat_id)
    }

    fn for_input(&mut self, input_id: &ObjectId) -> Option<&mut SeatInput> {
        self.input_by_seat
            .values_mut()
            .find(|entry| entry.input.id() == *input_id)
    }
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

    pub(super) fn get_text_input_for_surface(&self, surface: &WlSurface) -> Option<ZwpTextInputV3> {
        let inner = self.lock_inner("surface lookup")?;
        let surface_id = surface.id();
        inner
            .input_by_seat
            .values()
            .find(|entry| {
                entry
                    .focus
                    .active
                    .as_ref()
                    .is_some_and(|focus| focus.target.surface_id == surface_id)
            })
            .map(|entry| entry.input.clone())
    }

    pub(super) fn advise_seat(
        &self,
        seat: &WlSeat,
        keyboard: &WlKeyboard,
        qh: &QueueHandle<WaylandState>,
    ) {
        let Some(mut inner) = self.lock_inner("seat advice") else {
            return;
        };
        if inner.shutdown {
            return;
        }
        inner
            .input_by_seat
            .entry(seat.id())
            .or_insert_with(|| SeatInput {
                input: self
                    .text_input_manager
                    .get_text_input(seat, qh, TextInputData {}),
                focus: InputFocus::default(),
            });
        inner.keyboard_to_seat.insert(keyboard.id(), seat.id());
    }

    fn keyboard_focus(
        &self,
        keyboard: &WlKeyboard,
        target: Option<FocusTarget<ObjectId>>,
    ) -> Vec<FocusTarget<ObjectId>> {
        let Some(mut inner) = self.lock_inner("keyboard focus") else {
            return Vec::new();
        };
        let Some(entry) = inner.for_keyboard(keyboard) else {
            return Vec::new();
        };
        entry
            .focus
            .keyboard(target)
            .map(|change| entry.apply_focus_change(change))
            .unwrap_or_default()
    }

    fn forget_surface_id(&self, surface_id: &ObjectId) -> Vec<FocusTarget<ObjectId>> {
        let Some(mut inner) = self.lock_inner("surface removal") else {
            return Vec::new();
        };
        let mut reset = Vec::new();
        for entry in inner.input_by_seat.values_mut() {
            if let Some(change) = entry.focus.forget_surface(surface_id) {
                reset.extend(entry.apply_focus_change(change));
            }
        }
        reset
    }

    fn forget_keyboard(&self, keyboard: &WlKeyboard) -> Vec<FocusTarget<ObjectId>> {
        let Some(mut inner) = self.lock_inner("keyboard removal") else {
            return Vec::new();
        };
        let Some(seat_id) = inner.keyboard_to_seat.remove(&keyboard.id()) else {
            return Vec::new();
        };
        let Some(entry) = inner.input_by_seat.get_mut(&seat_id) else {
            return Vec::new();
        };
        entry
            .focus
            .keyboard(None)
            .map(|change| entry.apply_focus_change(change))
            .unwrap_or_default()
    }

    fn forget_seat(&self, seat: &WlSeat) -> Vec<FocusTarget<ObjectId>> {
        let Some(mut inner) = self.lock_inner("seat removal") else {
            return Vec::new();
        };
        let seat_id = seat.id();
        inner
            .keyboard_to_seat
            .retain(|_, mapped_seat| mapped_seat != &seat_id);
        inner
            .input_by_seat
            .remove(&seat_id)
            .and_then(SeatInput::destroy)
            .into_iter()
            .collect()
    }

    fn is_current(&self, input_id: &ObjectId, focus: &FocusCohort<ObjectId>) -> bool {
        let Some(mut inner) = self.lock_inner("batch delivery") else {
            return false;
        };
        inner
            .for_input(input_id)
            .is_some_and(|entry| entry.focus.active.as_ref() == Some(focus))
    }

    /// Workaround for <https://gitlab.gnome.org/GNOME/gnome-shell/-/issues/4776>.
    /// Disable and destroy each per-seat proxy before destroying the manager.
    fn shutdown(&self) {
        // Poison recovery is only for retiring resources, never for accepting
        // events or reviving delivery authority.
        let mut inner = match self.inner.lock() {
            Ok(inner) => inner,
            Err(poisoned) => {
                log::error!("Recovering poisoned Wayland text input state for shutdown");
                poisoned.into_inner()
            }
        };
        if inner.shutdown {
            return;
        }
        inner.shutdown = true;
        inner.keyboard_to_seat.clear();
        for (_, entry) in inner.input_by_seat.drain() {
            let _ = entry.destroy();
        }
        self.text_input_manager.destroy();
    }
}

pub(super) struct TextInputData {}

impl Dispatch<ZwpTextInputManagerV3, GlobalData, WaylandState> for TextInputState {
    fn event(
        _state: &mut WaylandState,
        _proxy: &ZwpTextInputManagerV3,
        _event: <ZwpTextInputManagerV3 as Proxy>::Event,
        _data: &GlobalData,
        _conn: &wayland_client::Connection,
        _qhandle: &QueueHandle<WaylandState>,
    ) {
        // No events from ZwpTextInputManagerV3.
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
        let entered = match &event {
            TextInputEvent::Enter { surface } => state.text_input_target(surface),
            _ => None,
        };
        let (reset, batch) = {
            let Some(text_input) = state.text_input.as_ref() else {
                log::debug!("Ignoring Wayland text input event without text input state");
                return;
            };
            let Some(mut inner) = text_input.lock_inner("text input event") else {
                return;
            };
            let Some(entry) = inner.for_input(&input.id()) else {
                // Destroyed objects and late callbacks must not recreate state.
                log::debug!("Ignoring Wayland text input event from an unknown object");
                return;
            };
            let mut reset = Vec::new();
            let mut batch = None;
            match event {
                TextInputEvent::Enter { .. } => {
                    if entered.is_none() {
                        log::debug!("Ignoring Wayland text input Enter for an unknown surface");
                    }
                    let change = entry.focus.enter(entered);
                    reset = entry.apply_focus_change(change);
                }
                TextInputEvent::Leave { surface } => {
                    if let Some(change) = entry.focus.leave(&surface.id()) {
                        reset = entry.apply_focus_change(change);
                    } else {
                        log::debug!("Ignoring stale Wayland text input Leave");
                    }
                }
                TextInputEvent::PreeditString { text, .. } => {
                    if !entry.focus.preedit(text) {
                        log::debug!("Ignoring Wayland preedit without matching surface focus");
                    }
                }
                TextInputEvent::CommitString { text } => {
                    if !entry.focus.commit(text) {
                        log::debug!("Ignoring Wayland commit without matching surface focus");
                    }
                }
                TextInputEvent::Done { serial } => {
                    batch = entry.focus.done(serial);
                }
                _ => {
                    // Surrounding-text support is not advertised. Do not log
                    // event payloads, which may contain the user's composition.
                    log::trace!("Ignoring unsupported Wayland text input event");
                }
            }
            (reset, batch)
        };

        // Consume pending state before dispatch and release the IME lock.
        // A callback may close a window or retire focus; never reinsert its
        // former batch after the callback, and recheck authority per event.
        state.reset_text_input_targets(reset);
        if let Some(batch) = batch {
            state.dispatch_text_input_batch(&input.id(), batch);
        }
    }
}

impl WaylandState {
    fn text_input_target(&self, surface: &WlSurface) -> Option<FocusTarget<ObjectId>> {
        let window_id = SurfaceUserData::try_from_wl(surface)?.window_id;
        let window = self.window_by_id(window_id)?;
        let surface_id = surface.id();
        if !window.borrow().has_text_input_surface(&surface_id) {
            return None;
        }
        Some(FocusTarget {
            surface_id,
            window_id,
        })
    }

    pub(super) fn advise_keyboard_text_input(&self, surface: &WlSurface, keyboard: &WlKeyboard) {
        let target = self.text_input_target(surface);
        if let Some(text_input) = &self.text_input {
            let reset = text_input.keyboard_focus(keyboard, target);
            self.reset_text_input_targets(reset);
        }
    }

    pub(super) fn disable_keyboard_text_input(&self, keyboard: &WlKeyboard) {
        if let Some(text_input) = &self.text_input {
            let reset = text_input.keyboard_focus(keyboard, None);
            self.reset_text_input_targets(reset);
        }
    }

    pub(super) fn retire_text_input_for_window_close(
        &self,
        clear_keyboard: bool,
        surface_id: Option<&ObjectId>,
    ) -> Vec<TextInputWindowReset> {
        let Some(text_input) = &self.text_input else {
            return Vec::new();
        };
        let mut targets = Vec::new();
        if clear_keyboard {
            if let Some(keyboard) = &self.keyboard {
                targets.extend(text_input.keyboard_focus(keyboard, None));
            }
        }
        if let Some(surface_id) = surface_id {
            targets.extend(text_input.forget_surface_id(surface_id));
        }
        // Window close holds the connection's mutable WaylandState borrow.
        // Capture exact handles here, but defer callbacks until it is released.
        self.capture_text_input_resets(targets)
    }

    pub(super) fn forget_text_input_keyboard(&self, keyboard: &WlKeyboard) {
        if let Some(text_input) = &self.text_input {
            let reset = text_input.forget_keyboard(keyboard);
            self.reset_text_input_targets(reset);
        }
    }

    pub(super) fn forget_text_input_seat(&self, seat: &WlSeat) {
        if let Some(text_input) = &self.text_input {
            let reset = text_input.forget_seat(seat);
            self.reset_text_input_targets(reset);
        }
    }

    fn capture_text_input_resets(
        &self,
        targets: Vec<FocusTarget<ObjectId>>,
    ) -> Vec<TextInputWindowReset> {
        targets
            .into_iter()
            .filter_map(|target| {
                self.window_by_id(target.window_id)
                    .map(|window| TextInputWindowReset {
                        window,
                        surface_id: target.surface_id,
                    })
            })
            .collect()
    }

    fn reset_text_input_targets(&self, targets: Vec<FocusTarget<ObjectId>>) {
        for reset in self.capture_text_input_resets(targets) {
            // The old window may no longer have keyboard focus, but
            // its visible preedit still needs to be retired.
            reset.apply();
        }
    }

    fn dispatch_text_input_event(
        &self,
        input_id: &ObjectId,
        focus: &FocusCohort<ObjectId>,
        event: WindowEvent,
    ) -> bool {
        if !self
            .text_input
            .as_ref()
            .is_some_and(|input| input.is_current(input_id, focus))
            || self.keyboard_window_id != Some(focus.target.window_id)
            || self.keyboard_active_surface_id.borrow().as_ref() != Some(&focus.target.surface_id)
        {
            log::debug!("Dropping Wayland text input batch whose focus was retired");
            return false;
        }
        let Some(window) = self.window_by_id(focus.target.window_id) else {
            return false;
        };
        let mut window = window.borrow_mut();
        if !window.has_text_input_surface(&focus.target.surface_id) {
            return false;
        }
        window.events.dispatch(event);
        true
    }

    fn dispatch_text_input_batch(&self, input_id: &ObjectId, batch: PendingBatch<ObjectId>) {
        // Apply the protocol's Done order: retire prior preedit, insert the
        // committed text, then show the new preedit. A serial mismatch does
        // not alter this order or authorize seat-global operations.
        if !self.dispatch_text_input_event(
            input_id,
            &batch.focus,
            WindowEvent::AdviseDeadKeyStatus(DeadKeyStatus::None),
        ) {
            return;
        }
        if let Some(text) = batch.state.commit.filter(|text| !text.is_empty()) {
            if !self.dispatch_text_input_event(
                input_id,
                &batch.focus,
                WindowEvent::KeyEvent(KeyEvent {
                    key: KeyCode::composed(&text),
                    modifiers: Modifiers::NONE,
                    leds: KeyboardLedStatus::empty(),
                    repeat_count: 1,
                    key_is_down: true,
                    raw: None,
                }),
            ) {
                return;
            }
        }
        if let Some(text) = batch.state.pre_edit.filter(|text| !text.is_empty()) {
            self.dispatch_text_input_event(
                input_id,
                &batch.focus,
                WindowEvent::AdviseDeadKeyStatus(DeadKeyStatus::Composing(text)),
            );
        }
    }
}

impl Drop for WaylandState {
    fn drop(&mut self) {
        if let Some(text_input) = &self.text_input {
            // Retire proxies and pending batches without calling window
            // handlers while their owning WaylandState is being destroyed.
            text_input.shutdown();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{FocusTarget, InputFocus};

    fn target(surface_id: u32) -> FocusTarget<u32> {
        FocusTarget {
            surface_id,
            window_id: surface_id as usize + 100,
        }
    }

    fn focused(surface_id: u32) -> InputFocus<u32> {
        let mut focus = InputFocus::default();
        let _ = focus.keyboard(Some(target(surface_id)));
        focus.enter(Some(target(surface_id)));
        focus
    }

    #[test]
    fn ime_batch_from_a_cannot_be_committed_into_b() {
        let mut focus = focused(1);
        assert!(focus.commit(Some("a".into())));
        let old = focus.active.clone().unwrap();
        let change = focus.keyboard(Some(target(2))).unwrap();
        assert_eq!(change.previous, Some(target(1)));
        assert!(change.disable);
        assert!(focus.pending.is_none());
        assert!(focus.done(3).is_none());
        focus.enter(Some(target(2)));
        assert_ne!(focus.active.as_ref().unwrap().generation, old.generation);
        assert!(focus.done(3).is_none());
        assert!(focus.commit(Some("b".into())));
        let batch = focus.done(4).unwrap();
        assert_eq!(batch.focus.target, target(2));
        assert_eq!(batch.state.commit.as_deref(), Some("b"));
    }

    #[test]
    fn ime_enable_waits_for_both_protocol_and_keyboard_focus_in_either_order() {
        let mut keyboard_first = InputFocus::default();
        assert!(
            keyboard_first
                .keyboard(Some(target(1)))
                .unwrap()
                .current
                .is_none()
        );
        assert!(!keyboard_first.commit(Some("too early".into())));
        assert_eq!(
            keyboard_first.enter(Some(target(1))).current,
            Some(target(1))
        );

        let mut protocol_first = InputFocus::default();
        assert!(protocol_first.enter(Some(target(1))).current.is_none());
        assert!(!protocol_first.preedit(Some("too early".into())));
        assert_eq!(
            protocol_first.keyboard(Some(target(1))).unwrap().current,
            Some(target(1))
        );
    }

    #[test]
    fn ime_leave_discards_partial_batch_and_post_leave_events() {
        let mut focus = focused(1);
        assert!(focus.preedit(Some("pending".into())));
        assert!(focus.commit(Some("uncommitted".into())));
        let change = focus.leave(&1).unwrap();
        assert_eq!(change.previous, Some(target(1)));
        assert!(!change.disable);
        assert!(focus.pending.is_none());
        assert!(!focus.commit(Some("late".into())));
        assert!(!focus.preedit(Some("late".into())));
        assert!(focus.done(99).is_none());
    }

    #[test]
    fn ime_stale_leave_and_surface_close_preserve_the_new_focus() {
        let mut focus = focused(1);
        let _ = focus.leave(&1);
        let _ = focus.keyboard(Some(target(2)));
        focus.enter(Some(target(2)));
        let current = focus.active.clone();
        assert!(focus.commit(Some("new window".into())));
        assert!(focus.leave(&1).is_none());
        assert!(focus.forget_surface(&1).is_none());
        assert_eq!(focus.active, current);
        assert_eq!(
            focus.done(7).unwrap().state.commit.as_deref(),
            Some("new window")
        );
    }

    #[test]
    fn ime_disable_and_reenable_same_surface_use_distinct_cohorts() {
        let mut focus = focused(1);
        assert!(focus.preedit(Some("old".into())));
        let old = focus.active.clone().unwrap();
        let disabled = focus.keyboard(None).unwrap();
        assert!(disabled.disable);
        assert!(focus.pending.is_none());
        assert!(focus.active.is_none());
        let _ = focus.keyboard(Some(target(1)));
        assert_ne!(focus.active.as_ref().unwrap().generation, old.generation);
        assert!(focus.done(1).is_none());
        focus.enter(Some(target(1)));
        assert_ne!(focus.active.as_ref().unwrap().generation, old.generation);
    }

    #[test]
    fn ime_batches_apply_only_at_done_and_are_consumed_once() {
        let mut focus = focused(1);
        for serial in [0, 0, u32::MAX, 1] {
            assert!(focus.commit(Some("committed".into())));
            assert!(!focus.preedit_visible);
            assert!(focus.pending.is_some());
            let batch = focus.done(serial).unwrap();
            assert_eq!(batch.state.commit.as_deref(), Some("committed"));
            assert!(focus.pending.is_none());
            assert!(focus.done(serial).is_none());
        }
    }

    #[test]
    fn ime_empty_done_removes_preedit_without_repeating_commit() {
        let mut focus = focused(1);
        assert!(focus.preedit(Some("composition".into())));
        assert!(!focus.preedit_visible);
        assert_eq!(
            focus.done(1).unwrap().state.pre_edit.as_deref(),
            Some("composition")
        );
        assert!(focus.preedit_visible);
        assert!(focus.commit(Some("finished".into())));
        assert!(focus.preedit_visible, "CommitString must wait for Done");
        let committed = focus.done(2).unwrap();
        assert_eq!(committed.state.commit.as_deref(), Some("finished"));
        assert!(committed.state.pre_edit.is_none());
        assert!(!focus.preedit_visible);
        assert!(focus.done(2).is_none());

        focus.preedit(Some("next".into()));
        let _ = focus.done(3);
        let cleared = focus.done(4).unwrap();
        assert!(cleared.state.pre_edit.is_none());
        assert!(cleared.state.commit.is_none());
        assert!(!focus.preedit_visible);
        assert!(focus.done(4).is_none());
    }

    #[test]
    fn ime_unknown_enter_and_current_surface_close_retire_authority() {
        let mut focus = focused(1);
        focus.preedit(Some("old".into()));
        let unknown = focus.enter(None);
        assert_eq!(unknown.previous, Some(target(1)));
        assert!(unknown.current.is_none());
        assert!(focus.pending.is_none());
        assert!(!focus.commit(Some("unknown".into())));
        assert!(focus.done(1).is_none());

        focus.enter(Some(target(1)));
        focus.commit(Some("closing".into()));
        let closed = focus.forget_surface(&1).unwrap();
        assert!(closed.disable);
        assert!(focus.protocol.is_none());
        assert!(focus.keyboard.is_none());
        assert!(focus.active.is_none());
        assert!(focus.done(2).is_none());
    }

    #[test]
    fn ime_generation_exhaustion_never_reuses_a_cohort() {
        let mut focus = focused(1);
        focus.generation = u64::MAX;
        focus.commit(Some("retired".into()));
        let change = focus.enter(Some(target(1)));
        assert!(change.disable);
        assert!(change.current.is_none());
        assert!(focus.exhausted);
        assert!(focus.pending.is_none());
        assert!(focus.done(1).is_none());
        let _ = focus.keyboard(None);
        let _ = focus.keyboard(Some(target(1)));
        focus.enter(Some(target(1)));
        assert!(focus.active.is_none());
        assert!(!focus.commit(Some("must stay retired".into())));
    }
}
