use crate::TermWindow;
use crate::termwindow::TermWindowNotif;
use config::keyassignment::{ClipboardCopyDestination, ClipboardPasteSource};
use mux::Mux;
use mux::pane::Pane;
use std::sync::Arc;
use window::{Clipboard, WindowOps};

impl TermWindow {
    pub(crate) fn show_osc52_prompt(
        &mut self,
        target: mux::Osc52PromptTarget,
        request: wezterm_term::Osc52ClipboardRequest,
        mux_owner: std::sync::Weak<Mux>,
        frontend: std::sync::Weak<()>,
        exact_window: window::Window,
    ) {
        let can_show = self.window.as_ref() == Some(&exact_window)
            && self.mux_window_id == target.window_id()
            && crate::frontend::osc52_frontend_is_current(&frontend)
            && target.is_current()
            && request.is_pending();
        let Some(owner) = mux_owner
            .upgrade()
            .filter(|owner| target.is_for_owner(owner))
        else {
            request.cancel();
            return;
        };
        let prior_consent_overlay = self
            .pane_state
            .borrow()
            .get(&target.pane_id())
            .and_then(|state| state.overlay.as_ref())
            .filter(|overlay| overlay.osc52_request.is_some())
            .map(|overlay| overlay.pane.pane_id());
        let visible = self.get_panes_to_render().iter().any(|pane| {
            pane.pane.pane_id() == target.pane_id()
                || Some(pane.pane.pane_id()) == prior_consent_overlay
        });
        if !can_show || !visible {
            log::warn!(
                "OSC 52 consent unavailable request={}: originating pane/window is not visible/current",
                request.id()
            );
            request.cancel();
            return;
        }
        // Never replace a user-owned copy/selection/other overlay for shell output.
        if self
            .pane_state
            .borrow()
            .get(&target.pane_id())
            .and_then(|state| state.overlay.as_ref())
            .is_some_and(|overlay| overlay.osc52_request.is_none())
        {
            request.cancel();
            return;
        }
        let Some(pane) = owner.get_pane(target.pane_id()) else {
            request.cancel();
            return;
        };
        if !owner
            .capture_pane_registration(&pane)
            .is_some_and(|registration| registration.same_registration(target.registration()))
        {
            request.cancel();
            return;
        }
        let completion_reservation = match promise::spawn::try_reserve_main_thread(
            promise::spawn::MainThreadServiceClass::Input,
            crate::overlay::OVERLAY_MAIN_THREAD_ESTIMATED_BYTES,
        ) {
            promise::spawn::MainThreadReservationOutcome::Reserved(reservation) => reservation,
            _ => {
                request.cancel();
                log::warn!(
                    "OSC 52 completion observer admission failed request={}",
                    request.id()
                );
                return;
            }
        };
        let operation = if request.is_clear() {
            "clear"
        } else if request.decoded_bytes() == 0 {
            "set an empty value in"
        } else {
            "replace"
        };
        let message = format!(
            "Pane {} asks to {operation} {:?} ({} bytes). Allow once? Clipboard contents are hidden.",
            target.pane_id(),
            request.selection(),
            request.decoded_bytes(),
        );
        let ticket_cell = Arc::new(std::sync::OnceLock::<super::OverlayCancellationTicket>::new());
        let worker_ticket = Arc::clone(&ticket_cell);
        let worker_request = request.clone();
        let worker_target = target.clone();
        let worker_window = exact_window.clone();
        let result = crate::overlay::start_bounded_overlay_pane(
            self,
            &pane,
            request.deadline(),
            move |_pane_id, mut term| {
                let confirmed = crate::overlay::confirm::run_confirmation_until(
                    &message,
                    &mut term,
                    worker_request.deadline(),
                    || {
                        worker_request.is_pending()
                            && worker_target.is_current()
                            && worker_ticket
                                .get()
                                .is_none_or(|ticket| !ticket.cancellation_requested())
                    },
                );
                if !matches!(confirmed, Ok(true)) {
                    worker_request.cancel();
                    if confirmed.is_err() {
                        log::warn!(
                            "OSC 52 confirmation input/render unavailable request={}",
                            worker_request.id()
                        );
                    }
                    return confirmed.map(|_| ());
                }
                let ticket = worker_ticket
                    .get()
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("OSC 52 overlay was never assigned"))?;
                let reservation = match promise::spawn::try_reserve_main_thread(
                    promise::spawn::MainThreadServiceClass::Input,
                    crate::overlay::OVERLAY_MAIN_THREAD_ESTIMATED_BYTES,
                ) {
                    promise::spawn::MainThreadReservationOutcome::Reserved(reservation) => {
                        reservation
                    }
                    _ => {
                        worker_request.cancel();
                        anyhow::bail!("OSC 52 grant callback admission failed");
                    }
                };
                let (completion, receipt) = std::sync::mpsc::sync_channel(1);
                let callback_request = worker_request.clone();
                reservation
                    .spawn(async move {
                        let expected_window = worker_window.clone();
                        worker_window.notify(TermWindowNotif::Apply(Box::new(
                            move |term_window| {
                                let result = term_window.apply_osc52_consent(
                                    &worker_target,
                                    &callback_request,
                                    &ticket,
                                    &frontend,
                                    &expected_window,
                                );
                                if completion.try_send(result).is_err() {
                                    log::debug!(
                                        "OSC 52 grant callback receipt retired request={}",
                                        callback_request.id()
                                    );
                                }
                            },
                        )));
                    })
                    .detach();
                // Keep the exact overlay alive until its apply callback settles or
                // its original deadline expires. This ACK is the GUI enqueue result,
                // not an OS/application clipboard-delivery acknowledgement.
                match receipt.recv_timeout(
                    worker_request
                        .deadline()
                        .saturating_duration_since(std::time::Instant::now()),
                ) {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(error)) => Err(error.into()),
                    Err(_) => {
                        worker_request.cancel();
                        anyhow::bail!("OSC 52 grant callback expired or disconnected")
                    }
                }
            },
        );
        let (overlay, ticket, future) = match result {
            Ok(result) => result,
            Err(_) => {
                request.cancel();
                log::warn!("OSC 52 overlay admission failed request={}", request.id());
                return;
            }
        };
        if ticket_cell.set(ticket.clone()).is_err() {
            request.cancel();
            return;
        }
        self.assign_overlay_for_pane_with_ticket(target.pane_id(), overlay, ticket.clone());
        let mut attached = false;
        if let Some(overlay) = self.pane_state(target.pane_id()).overlay.as_mut() {
            if overlay.cancellation_ticket.matches(&ticket) {
                overlay.osc52_request = Some(request.clone());
                attached = true;
            }
        }
        if attached {
            log::info!(
                "OSC 52 consent overlay assigned request={} pane={} window={} generation={} bytes={} clear={}",
                request.id(),
                target.pane_id(),
                target.window_id(),
                target.structural_generation(),
                request.decoded_bytes(),
                request.is_clear()
            );
        } else {
            request.cancel();
            log::warn!("OSC 52 overlay assignment refused request={}", request.id());
        }
        completion_reservation
            .spawn_local(async move {
                if future.await.is_err() {
                    request.cancel();
                    log::warn!("OSC 52 overlay worker failed request={}", request.id());
                }
            })
            .detach();
        exact_window.invalidate();
    }

    fn apply_osc52_consent(
        &mut self,
        target: &mux::Osc52PromptTarget,
        request: &wezterm_term::Osc52ClipboardRequest,
        ticket: &super::OverlayCancellationTicket,
        frontend: &std::sync::Weak<()>,
        exact_window: &window::Window,
    ) -> Result<(), wezterm_term::Osc52PromptError> {
        let exact_overlay = self
            .pane_state
            .borrow()
            .get(&target.pane_id())
            .and_then(|state| state.overlay.as_ref())
            .is_some_and(|overlay| {
                overlay.cancellation_ticket.matches(ticket)
                    && overlay
                        .osc52_request
                        .as_ref()
                        .is_some_and(|current| current.id() == request.id())
            });
        if self.window.as_ref() != Some(exact_window)
            || self.mux_window_id != target.window_id()
            || !crate::frontend::osc52_frontend_is_current(frontend)
            || !exact_overlay
            || ticket.cancellation_requested()
            || !target.is_current()
        {
            request.cancel();
            return Err(wezterm_term::Osc52PromptError::Revoked);
        }
        request.apply_with(|selection, data| {
            target.with_current_until(request.deadline(), || {
                exact_window.set_clipboard(
                    match selection {
                        wezterm_term::ClipboardSelection::Clipboard => Clipboard::Clipboard,
                        wezterm_term::ClipboardSelection::PrimarySelection => {
                            Clipboard::PrimarySelection
                        }
                    },
                    data.unwrap_or_default(),
                );
            })
        })?;
        log::info!(
            "OSC 52 consent submitted_to_window request={} pane={} window={}; application ACK pending",
            request.id(),
            target.pane_id(),
            target.window_id()
        );
        Ok(())
    }

    pub fn copy_to_clipboard(&self, clipboard: ClipboardCopyDestination, text: String) {
        let Some(window) = self.window.as_ref() else {
            log::warn!("cannot copy to clipboard: GUI window is not attached");
            return;
        };

        let clipboard = match clipboard {
            ClipboardCopyDestination::Clipboard => [Some(Clipboard::Clipboard), None],
            ClipboardCopyDestination::PrimarySelection => [Some(Clipboard::PrimarySelection), None],
            ClipboardCopyDestination::ClipboardAndPrimarySelection => [
                Some(Clipboard::Clipboard),
                Some(Clipboard::PrimarySelection),
            ],
        };
        for &c in &clipboard {
            if let Some(c) = c {
                window.set_clipboard(c, text.clone());
            }
        }
    }

    pub fn paste_from_clipboard(&mut self, pane: &Arc<dyn Pane>, clipboard: ClipboardPasteSource) {
        let pane_id = pane.pane_id();
        log::trace!(
            "paste_from_clipboard in pane {} {:?}",
            pane.pane_id(),
            clipboard
        );
        let Some(window) = self.window.as_ref().cloned() else {
            log::warn!(
                "cannot paste from clipboard into pane {pane_id}: GUI window is not attached"
            );
            return;
        };
        let clipboard = match clipboard {
            ClipboardPasteSource::Clipboard => Clipboard::Clipboard,
            ClipboardPasteSource::PrimarySelection => Clipboard::PrimarySelection,
        };
        let reservation = match promise::spawn::try_reserve_main_thread(
            promise::spawn::MainThreadServiceClass::Input,
            8 * 1024,
        ) {
            promise::spawn::MainThreadReservationOutcome::Reserved(reservation) => reservation,
            rejected => {
                log::error!(
                    "main-thread scheduler rejected clipboard paste before clipboard request: {rejected:?}"
                );
                return;
            }
        };
        let future = window.get_clipboard(clipboard);
        reservation
            .spawn_local(async move {
                if let Ok(clip) = future.await {
                    window.notify(TermWindowNotif::Apply(Box::new(move |myself| {
                        if let Some(pane) = myself
                            .pane_state(pane_id)
                            .overlay
                            .as_ref()
                            .map(|overlay| overlay.pane.clone())
                            .or_else(|| Mux::try_get().and_then(|mux| mux.get_pane(pane_id)))
                        {
                            pane.send_paste(&clip).ok();
                        }
                    })));
                }
            })
            .detach();
        self.maybe_scroll_to_bottom_for_input(&pane);
    }
}
