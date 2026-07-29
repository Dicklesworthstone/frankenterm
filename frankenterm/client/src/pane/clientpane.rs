use crate::client::RpcGenerationScope;
use crate::domain::{lock_or_recover, ClientInner};
use crate::pane::mousestate::MouseState;
use crate::pane::renderable::{
    hydrate_lines, RenderableInner, RenderablePaneBinding, RenderableState,
};
use anyhow::bail;
use async_trait::async_trait;
use codec::*;
use config::configuration;
use config::keyassignment::ScrollbackEraseMode;
use mux::domain::DomainId;
use mux::pane::{
    CachePolicy, CloseReason, ForEachPaneLogicalLine, LogicalLine, Pane, PaneId, Pattern,
    SearchResult, WithPaneLines,
};
use mux::renderable::{RenderableDimensions, StableCursorPosition};
use mux::tab::TabId;
use mux::PaneRegistrationSlot;
use parking_lot::{MappedMutexGuard, Mutex, MutexGuard};
use rangeset::RangeSet;
use ratelim::RateLimiter;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::convert::TryFrom;
use std::ops::Range;
use std::sync::Arc;
use termwiz::input::KeyEvent;
use termwiz::surface::SequenceNo;
use url::Url;
use wezterm_dynamic::Value;
use wezterm_term::color::ColorPalette;
use wezterm_term::{
    Alert, Clipboard, KeyCode, KeyModifiers, Line, MouseEvent, Progress, StableRowIndex,
    TerminalConfiguration, TerminalSize,
};

pub struct ClientPane {
    client: Arc<ClientInner>,
    local_pane_id: PaneId,
    pub remote_pane_id: PaneId,
    pub remote_tab_id: TabId,
    pub renderable: Arc<Mutex<RenderableState>>,
    configured_palette: Mutex<ColorPalette>,
    palette: Mutex<ColorPalette>,
    application_palette: Mutex<bool>,
    writer: Mutex<PaneWriter>,
    mouse: Arc<Mutex<MouseState>>,
    clipboard: Mutex<Option<Arc<dyn Clipboard>>>,
    mouse_grabbed: Mutex<bool>,
    alt_screen_active: Mutex<bool>,
    ignore_next_kill: Mutex<bool>,
    user_vars: Mutex<HashMap<String, String>>,
    config: Mutex<Option<Arc<dyn TerminalConfiguration>>>,
    unseen_output: Mutex<bool>,
    progress: Mutex<Progress>,
    mux_registration: Arc<PaneRegistrationSlot>,
}

impl ClientPane {
    pub(crate) fn new(
        client: &Arc<ClientInner>,
        local_pane_id: PaneId,
        remote_tab_id: TabId,
        remote_pane_id: PaneId,
        size: TerminalSize,
        title: &str,
        alt_screen_active: bool,
    ) -> Self {
        let writer = PaneWriter {
            client: Arc::clone(client),
            remote_pane_id,
        };

        let mouse = Arc::new(Mutex::new(MouseState::new(
            remote_pane_id,
            client.client.clone(),
        )));

        let fetch_limiter =
            RateLimiter::new(|config| config.ratelimit_mux_line_prefetches_per_second);

        let mux_registration = Arc::new(PaneRegistrationSlot::default());
        let renderable = Arc::new_cyclic(|weak_renderable| {
            Mutex::new(RenderableState {
                inner: RefCell::new(RenderableInner::new(
                    RenderablePaneBinding::new(
                        client,
                        remote_pane_id,
                        local_pane_id,
                        Arc::clone(&mux_registration),
                    ),
                    RenderableDimensions {
                        cols: size.cols as _,
                        viewport_rows: size.rows as _,
                        scrollback_rows: size.rows as _,
                        physical_top: 0,
                        scrollback_top: 0,
                        dpi: size.dpi,
                        pixel_width: size.pixel_width,
                        pixel_height: size.pixel_height,
                        reverse_video: false,
                    },
                    title,
                    fetch_limiter,
                    weak_renderable.clone(),
                )),
            })
        });

        let config = configuration();
        let palette: ColorPalette = config.resolved_palette.clone().into();

        Self {
            client: Arc::clone(client),
            mouse,
            remote_pane_id,
            local_pane_id,
            remote_tab_id,
            application_palette: Mutex::new(false),
            renderable,
            writer: Mutex::new(writer),
            configured_palette: Mutex::new(palette.clone()),
            palette: Mutex::new(palette),
            clipboard: Mutex::new(None),
            mouse_grabbed: Mutex::new(false),
            alt_screen_active: Mutex::new(alt_screen_active),
            ignore_next_kill: Mutex::new(false),
            unseen_output: Mutex::new(false),
            user_vars: Mutex::new(HashMap::new()),
            config: Mutex::new(None),
            progress: Mutex::new(Progress::default()),
            mux_registration,
        }
    }

    pub(crate) async fn process_unilateral(
        &self,
        registration: &mux::PaneRegistrationHandle,
        rpc: &RpcGenerationScope,
        pdu: Pdu,
    ) -> anyhow::Result<()> {
        let registration_matches_self = registration
            .try_with_current(|current| current.is_same_pane_ref(self))
            .unwrap_or(false);
        if !registration_matches_self {
            log::trace!(
                "discarding unilateral PDU for mismatched or stale client pane registration {}",
                self.local_pane_id
            );
            return Ok(());
        }

        match pdu {
            Pdu::GetPaneRenderChangesResponse(mut delta) => {
                let mouse_grabbed = delta.mouse_grabbed;
                let alt_screen_active = delta.alt_screen_active;
                let is_new_enough = registration
                    .try_with_current(|_| {
                        let renderable = self.renderable.lock();
                        renderable.get_current_seqno() <= delta.seqno
                    })
                    .unwrap_or(false);
                if !is_new_enough {
                    return Ok(());
                }

                let bonus_lines = std::mem::take(&mut delta.bonus_lines);
                let bonus_lines = hydrate_lines(rpc, delta.pane_id, bonus_lines).await;

                let applied = registration
                    .try_with_current_output(|_| {
                        let applied = self
                            .renderable
                            .lock()
                            .inner
                            .borrow_mut()
                            .apply_changes_to_surface(delta, bonus_lines);
                        if applied {
                            *self.mouse_grabbed.lock() = mouse_grabbed;
                            *self.alt_screen_active.lock() = alt_screen_active;
                        }
                        applied
                    })
                    .unwrap_or(false);
                if !applied {
                    log::trace!(
                        "discarding render delta for stale client pane registration {}",
                        self.local_pane_id
                    );
                }
            }
            Pdu::SetClipboard(SetClipboard {
                clipboard,
                selection,
                ..
            }) => {
                if let Some(result) = registration.try_with_current(|current| {
                    if !current.is_same_pane_ref(self) {
                        return Ok(());
                    }
                    let clipboard_handler = { self.clipboard.lock().clone() };
                    match clipboard_handler {
                        Some(clip) => {
                            log::debug!(
                                "Pdu::SetClipboard pane={} remote={} {:?} {:?}",
                                self.local_pane_id,
                                self.remote_pane_id,
                                selection,
                                clipboard
                            );
                            clip.set_contents(selection, clipboard)
                        }
                        None => {
                            log::error!(
                                "ClientPane: Ignoring SetClipboard request {:?}",
                                clipboard
                            );
                            Ok(())
                        }
                    }
                }) {
                    result?;
                }
            }
            Pdu::SetPalette(SetPalette { palette, .. }) => {
                let _ = registration.try_with_current(|current| {
                    *self.application_palette.lock() = palette != *self.configured_palette.lock();
                    *self.palette.lock() = palette;
                    self.renderable.lock().inner.borrow_mut().make_all_stale();
                    current.dispatch_alert(Alert::PaletteChanged);
                });
            }
            Pdu::NotifyAlert(NotifyAlert { alert, .. }) => {
                let _ = registration.try_with_current(|current| {
                    match &alert {
                        Alert::SetUserVar { name, value } => {
                            self.user_vars.lock().insert(name.clone(), value.clone());
                        }
                        Alert::OutputSinceFocusLost => {
                            *self.unseen_output.lock() = true;
                        }
                        Alert::Progress(progress) => {
                            *self.progress.lock() = progress.clone();
                        }
                        _ => {}
                    }
                    current.dispatch_alert(alert);
                });
            }
            Pdu::PaneRemoved(PaneRemoved { pane_id }) => {
                log::trace!("remote pane {} has been removed", pane_id);
                let _ = registration.try_with_current(|current| {
                    self.renderable.lock().inner.borrow_mut().dead = true;
                    current.prune_dead_windows();
                    self.client.expire_stale_mappings(&current);
                });
            }
            Pdu::PaneFocused(PaneFocused { pane_id }) => {
                // We get here whenever the pane focus is changed on the
                // server. That might be due to the user here in the GUI
                // doing things, or it may be due to a "remote"
                // `wezterm cli activate-pane-direction` or similar call
                // from some other actor.
                // The latter case is the important one: it is desirable
                // for the focus change to be reflected locally after it
                // has been changed on the server, so we work to apply
                // it here.
                log::trace!("advised of remote pane focus: {pane_id}");

                let _ = registration.try_with_current(|current| {
                    if let Err(err) = current.focus_pane_and_containing_tab() {
                        log::error!("Error reconciling remote PaneFocused notification: {err:#}");
                    }
                });
            }
            _ => bail!("unhandled unilateral pdu: {:?}", pdu),
        };
        Ok(())
    }

    pub fn remote_pane_id(&self) -> PaneId {
        self.remote_pane_id
    }

    pub(crate) fn belongs_to_client(&self, client: &ClientInner) -> bool {
        std::ptr::eq(self.client.as_ref(), client)
    }

    /// Arrange to suppress the next Pane::kill call.
    /// This is a bit of a hack that we use when closing a window;
    /// our Domain::local_window_is_closing impl calls this for each
    /// ClientPane in the window so that closing a window effectively
    /// "detaches" the window so that reconnecting later will resume
    /// from where they left off.
    /// It isn't perfect.
    pub fn ignore_next_kill(&self) {
        *self.ignore_next_kill.lock() = true;
    }

    pub fn sync_remote_listing_state(&self, alt_screen_active: bool) {
        *self.alt_screen_active.lock() = alt_screen_active;
    }
}

#[async_trait(?Send)]
impl Pane for ClientPane {
    fn pane_id(&self) -> PaneId {
        self.local_pane_id
    }

    fn mux_registration_slot(&self) -> &Arc<PaneRegistrationSlot> {
        &self.mux_registration
    }

    fn mux_registration_did_bind(&self, registration: mux::PaneRegistrationHandle) {
        let registration_matches_self = registration
            .try_with_current(|current| current.is_same_pane_ref(self))
            .unwrap_or(false);
        if !registration_matches_self {
            log::trace!(
                "skipping client pane bind work for mismatched or stale registration {}",
                self.local_pane_id
            );
            return;
        }

        self.renderable
            .lock()
            .inner
            .borrow_mut()
            .registration_did_bind();

        // Advise the server only after the pane has acquired exact mux
        // registration authority. Re-check the pane-owned slot when the
        // detached task runs so work beginning after retirement/rebind is
        // discarded.
        let mux_registration = Arc::clone(&self.mux_registration);
        let client = Arc::clone(&self.client);
        let remote_pane_id = self.remote_pane_id;
        let palette = self.configured_palette.lock().clone();
        let request = client.client.set_configured_palette_for_pane(SetPalette {
            pane_id: remote_pane_id,
            palette,
        });
        promise::spawn::spawn(async move {
            let registration_is_current = mux_registration
                .load()
                .is_some_and(|current| current.same_registration(&registration))
                && registration.try_with_current(|_| ()).is_some();
            if !registration_is_current {
                return Ok(());
            }

            request.await.map(|_| ())
        })
        .detach();
    }

    fn get_metadata(&self) -> Value {
        let renderable = self.renderable.lock();
        let inner = renderable.inner.borrow();

        let mut map: BTreeMap<Value, Value> = BTreeMap::new();
        map.insert(
            Value::String("is_tardy".to_string()),
            Value::Bool(inner.is_tardy()),
        );
        map.insert(
            Value::String("since_last_response_ms".to_string()),
            Value::U64(
                u64::try_from(inner.last_recv_time.elapsed().as_millis()).unwrap_or(u64::MAX),
            ),
        );

        Value::Object(map.into())
    }

    fn get_cursor_position(&self) -> StableCursorPosition {
        self.renderable.lock().get_cursor_position()
    }

    fn get_dimensions(&self) -> RenderableDimensions {
        self.renderable.lock().get_dimensions()
    }

    fn with_lines_mut(&self, lines: Range<StableRowIndex>, with_lines: &mut dyn WithPaneLines) {
        mux::pane::impl_with_lines_via_get_lines(self, lines, with_lines);
    }

    fn for_each_logical_line_in_stable_range_mut(
        &self,
        lines: Range<StableRowIndex>,
        for_line: &mut dyn ForEachPaneLogicalLine,
    ) {
        mux::pane::impl_for_each_logical_line_via_get_logical_lines(self, lines, for_line);
    }

    fn get_lines(&self, lines: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
        self.renderable.lock().get_lines(lines)
    }

    fn get_logical_lines(&self, lines: Range<StableRowIndex>) -> Vec<LogicalLine> {
        mux::pane::impl_get_logical_lines_via_get_lines(self, lines)
    }

    fn get_current_seqno(&self) -> SequenceNo {
        self.renderable.lock().get_current_seqno()
    }

    fn get_changed_since(
        &self,
        lines: Range<StableRowIndex>,
        seqno: SequenceNo,
    ) -> RangeSet<StableRowIndex> {
        self.renderable.lock().get_changed_since(lines, seqno)
    }

    fn set_clipboard(&self, clipboard: &Arc<dyn Clipboard>) {
        self.clipboard.lock().replace(Arc::clone(clipboard));
    }

    fn get_title(&self) -> String {
        let renderable = self.renderable.lock();
        let inner = renderable.inner.borrow();
        inner.title.clone()
    }

    fn get_progress(&self) -> Progress {
        self.progress.lock().clone()
    }

    fn send_paste(&self, text: &str) -> anyhow::Result<()> {
        let client = Arc::clone(&self.client);
        let remote_pane_id = self.remote_pane_id;
        self.renderable
            .lock()
            .inner
            .borrow_mut()
            .predict_from_paste(text);

        let data = text.to_owned();
        let request = client.client.send_paste(SendPaste {
            pane_id: remote_pane_id,
            data,
        });
        promise::spawn::spawn(request).detach();
        self.renderable.lock().inner.borrow_mut().update_last_send();
        Ok(())
    }

    fn reader(&self) -> anyhow::Result<Option<Box<dyn std::io::Read + Send>>> {
        Ok(None)
    }

    fn writer(&self) -> MappedMutexGuard<'_, dyn std::io::Write> {
        MutexGuard::map(self.writer.lock(), |writer| {
            let w: &mut dyn std::io::Write = writer;
            w
        })
    }

    fn set_zoomed(&self, zoomed: bool) {
        let render = self.renderable.lock();
        let mut inner = render.inner.borrow_mut();
        let client = Arc::clone(&self.client);
        let remote_pane_id = self.remote_pane_id;
        let remote_tab_id = self.remote_tab_id;
        // Invalidate any cached rows on a resize
        inner.make_all_stale();
        let request = client.client.set_zoomed(SetPaneZoomed {
            containing_tab_id: remote_tab_id,
            pane_id: remote_pane_id,
            zoomed,
        });
        promise::spawn::spawn(request).detach();
        inner.update_last_send();
    }

    fn resize(&self, size: TerminalSize) -> anyhow::Result<()> {
        let render = self.renderable.lock();
        let mut inner = render.inner.borrow_mut();

        let cols = size.cols;
        let rows = size.rows;

        if inner.dimensions.cols != cols
            || inner.dimensions.viewport_rows != rows
            || inner.dimensions.pixel_width != size.pixel_width
            || inner.dimensions.pixel_height != size.pixel_height
        {
            inner.dimensions.cols = cols;
            inner.dimensions.viewport_rows = rows;
            inner.dimensions.pixel_width = size.pixel_width;
            inner.dimensions.pixel_height = size.pixel_height;

            // Invalidate any cached rows on a resize
            inner.make_all_stale();

            let client = Arc::clone(&self.client);
            let remote_pane_id = self.remote_pane_id;
            let remote_tab_id = self.remote_tab_id;
            let request = client.client.resize(Resize {
                containing_tab_id: remote_tab_id,
                pane_id: remote_pane_id,
                size,
            });
            promise::spawn::spawn(request).detach();
            inner.update_last_send();
        }
        Ok(())
    }

    async fn search(
        &self,
        pattern: Pattern,
        range: Range<StableRowIndex>,
        limit: Option<u32>,
    ) -> anyhow::Result<Vec<SearchResult>> {
        match self
            .client
            .client
            .search_scrollback(SearchScrollbackRequest {
                pane_id: self.remote_pane_id,
                pattern,
                range,
                limit,
            })
            .await
        {
            Ok(SearchScrollbackResponse { results }) => Ok(results),
            Err(e) => Err(e),
        }
    }

    fn key_down(&self, key: KeyCode, mods: KeyModifiers) -> anyhow::Result<()> {
        let input_serial;
        {
            let renderable = self.renderable.lock();
            let mut inner = renderable.inner.borrow_mut();
            inner.input_serial = InputSerial::now();
            input_serial = inner.input_serial;
            inner.predict_from_key_event(key, mods);
        }
        let client = Arc::clone(&self.client);
        let remote_pane_id = self.remote_pane_id;
        let request = client.client.key_down(SendKeyDown {
            pane_id: remote_pane_id,
            event: KeyEvent {
                key,
                modifiers: mods,
            },
            input_serial,
        });
        promise::spawn::spawn(request).detach();
        self.renderable.lock().inner.borrow_mut().update_last_send();
        Ok(())
    }

    fn key_up(&self, key: KeyCode, mods: KeyModifiers) -> anyhow::Result<()> {
        let client = Arc::clone(&self.client);
        let remote_pane_id = self.remote_pane_id;
        let request = client.client.key_up(SendKeyUp {
            pane_id: remote_pane_id,
            event: KeyEvent {
                key,
                modifiers: mods,
            },
        });
        promise::spawn::spawn(request).detach();
        Ok(())
    }

    fn kill(&self) {
        let mut ignore = self.ignore_next_kill.lock();
        if *ignore {
            *ignore = false;
            return;
        }
        let client = Arc::clone(&self.client);
        let remote_pane_id = self.remote_pane_id;

        // We only want to ask the server to kill the pane if the user
        // explicitly requested it to die.
        // Domain detaching can implicitly call Pane::kill on the panes
        // in the domain, so we need to check here whether the domain is
        // in the detached state; if so then we must skip sending the
        // kill to the server.
        if !client.is_detached() {
            let request = client.client.kill_pane(KillPane {
                pane_id: remote_pane_id,
            });
            promise::spawn::spawn(request).detach();
        }
    }

    fn mouse_event(&self, event: MouseEvent) -> anyhow::Result<()> {
        self.mouse.lock().append(event);
        if MouseState::next(Arc::clone(&self.mouse)) {
            self.renderable.lock().inner.borrow_mut().update_last_send();
        }
        Ok(())
    }

    fn is_dead(&self) -> bool {
        self.renderable.lock().inner.borrow().dead
    }

    fn palette(&self) -> ColorPalette {
        self.palette.lock().clone()
    }

    fn domain_id(&self) -> DomainId {
        self.client.local_domain_id
    }

    fn is_mouse_grabbed(&self) -> bool {
        *self.mouse_grabbed.lock()
    }

    fn is_alt_screen_active(&self) -> bool {
        *self.alt_screen_active.lock()
    }

    fn get_current_working_dir(&self, _policy: CachePolicy) -> Option<Url> {
        self.renderable.lock().inner.borrow().working_dir.clone()
    }

    fn focus_changed(&self, focused: bool) {
        if focused {
            self.advise_focus();
            *self.unseen_output.lock() = false;
        }
    }

    fn erase_scrollback(&self, erase_mode: ScrollbackEraseMode) {
        let client = Arc::clone(&self.client);
        let remote_pane_id = self.remote_pane_id;
        let request = client.client.erase_scrollback(EraseScrollbackRequest {
            pane_id: remote_pane_id,
            erase_mode,
        });
        promise::spawn::spawn(request).detach();
    }

    fn advise_focus(&self) {
        let mut focused_pane = lock_or_recover(
            &self.client.focused_remote_pane_id,
            "focused_remote_pane_id",
        );
        if *focused_pane != Some(self.remote_pane_id) {
            focused_pane.replace(self.remote_pane_id);
            let client = Arc::clone(&self.client);
            let remote_pane_id = self.remote_pane_id;
            let request = client.client.set_focused_pane_id(SetFocusedPane {
                pane_id: remote_pane_id,
            });
            promise::spawn::spawn(request).detach();
        }
    }

    fn has_unseen_output(&self) -> bool {
        *self.unseen_output.lock()
    }

    fn can_close_without_prompting(&self, reason: CloseReason) -> bool {
        match reason {
            CloseReason::Window => true,
            CloseReason::Tab => false,
            CloseReason::Pane => false,
        }
    }

    fn copy_user_vars(&self) -> HashMap<String, String> {
        self.user_vars.lock().clone()
    }

    fn set_config(&self, config: Arc<dyn TerminalConfiguration>) {
        let palette = config.color_palette();
        // If the application running in the pane hasn't changed the
        // palette through escape sequences, speculatively adopt the
        // new palette so that it updates with the lowest latency.
        if !*self.application_palette.lock() {
            *self.palette.lock() = palette.clone();
        }
        *self.configured_palette.lock() = palette.clone();

        // and now send the color palette to the server
        let client = Arc::clone(&self.client);
        let remote_pane_id = self.remote_pane_id;
        let request = client.client.set_configured_palette_for_pane(SetPalette {
            pane_id: remote_pane_id,
            palette,
        });
        promise::spawn::spawn(request).detach();
        self.config.lock().replace(config);
    }

    fn get_config(&self) -> Option<Arc<dyn TerminalConfiguration>> {
        self.config.lock().clone()
    }
}

struct PaneWriter {
    client: Arc<ClientInner>,
    remote_pane_id: PaneId,
}

impl std::io::Write for PaneWriter {
    fn write(&mut self, data: &[u8]) -> Result<usize, std::io::Error> {
        // Invoked on the GUI main thread when the user types into a remote pane
        // (macOS `Key::Composed`/IME, kitty-keyboard, win32-input-mode, SendString,
        // charselect). It MUST NOT block: the previous `block_on_io(write_to_pane)`
        // parked the main thread on the RPC reply, and because `send_pdu` has no
        // timeout, typing into a pane on a slow or dead/reconnecting domain froze
        // the ENTIRE GUI — every pane stopped rendering and no other domain was
        // serviced (a head-of-line block; visible in profiles as the main thread in
        // `__psynch_cvwait`/`semaphore_wait`). Mirror the non-blocking sibling input
        // methods (`key_down`, `send_paste`, `resize`): fire-and-forget on the
        // runtime and report the bytes as accepted. Ordering is preserved — spawned
        // tasks run FIFO on the main-thread spawn queue and each enqueues its PDU
        // into the ordered channel before its first await, exactly as `key_down`
        // already relies on.
        let client = Arc::clone(&self.client);
        let pane_id = self.remote_pane_id;
        let data = data.to_vec();
        let len = data.len();
        let request = client.client.write_to_pane(WriteToPane { pane_id, data });
        promise::spawn::spawn(request).detach();
        Ok(len)
    }

    fn flush(&mut self) -> Result<(), std::io::Error> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Client;
    use crate::domain::ClientDomainConfig;
    use crate::MuxTestScope;
    use config::UnixDomain;
    use mux::renderable::{RenderableDimensions, StableCursorPosition};
    use mux::{Mux, MuxNotification};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex as StdMutex;

    fn test_client_inner(local_domain_id: DomainId) -> Arc<ClientInner> {
        let unix = UnixDomain {
            name: "client-pane-test".to_string(),
            ..UnixDomain::default()
        };
        Arc::new(ClientInner::new(
            local_domain_id,
            Client::new_test_client(Some(local_domain_id), ClientDomainConfig::Unix(unix)),
            None,
            None,
            false,
        ))
    }

    fn test_client_pane(
        inner: &Arc<ClientInner>,
        local_pane_id: PaneId,
        remote_pane_id: PaneId,
    ) -> Arc<ClientPane> {
        Arc::new(ClientPane::new(
            inner,
            local_pane_id,
            23,
            remote_pane_id,
            TerminalSize {
                cols: 80,
                rows: 24,
                pixel_width: 800,
                pixel_height: 480,
                dpi: 96,
            },
            "shell",
            false,
        ))
    }

    struct NoopClipboard;

    impl Clipboard for NoopClipboard {
        fn set_contents(
            &self,
            _selection: wezterm_term::ClipboardSelection,
            _data: Option<String>,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    struct ReentrantClipboard {
        pane: std::sync::Weak<ClientPane>,
        callback_ran: Arc<AtomicBool>,
    }

    impl Clipboard for ReentrantClipboard {
        fn set_contents(
            &self,
            _selection: wezterm_term::ClipboardSelection,
            _data: Option<String>,
        ) -> anyhow::Result<()> {
            let pane = self
                .pane
                .upgrade()
                .ok_or_else(|| anyhow::anyhow!("test client pane was dropped"))?;
            let clipboard_guard = pane.clipboard.try_lock().ok_or_else(|| {
                anyhow::anyhow!("clipboard callback ran while the ClientPane mutex was held")
            })?;
            drop(clipboard_guard);

            let replacement: Arc<dyn Clipboard> = Arc::new(NoopClipboard);
            pane.set_clipboard(&replacement);
            self.callback_ran.store(true, Ordering::Release);
            Ok(())
        }
    }

    #[test]
    fn render_delta_updates_authoritative_alt_screen_state() {
        let scope = MuxTestScope::enter();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);
        let inner = test_client_inner(17);
        let pane = Arc::new(ClientPane::new(
            &inner,
            31,
            23,
            29,
            TerminalSize {
                cols: 80,
                rows: 24,
                pixel_width: 800,
                pixel_height: 480,
                dpi: 96,
            },
            "shell",
            false,
        ));
        let pane_for_mux: Arc<dyn Pane> = pane.clone();
        mux.add_pane(&pane_for_mux)
            .expect("render-delta test pane should register");
        let registration = mux
            .capture_pane_registration(&pane_for_mux)
            .expect("render-delta test pane should retain exact registration");
        let rpc = inner.client.rpc_scope();
        assert!(!pane.is_alt_screen_active());

        promise::spawn::block_on(async {
            pane.process_unilateral(
                &registration,
                &rpc,
                Pdu::GetPaneRenderChangesResponse(GetPaneRenderChangesResponse {
                    pane_id: 29,
                    mouse_grabbed: false,
                    alt_screen_active: true,
                    cursor_position: StableCursorPosition::default(),
                    dimensions: RenderableDimensions {
                        cols: 80,
                        viewport_rows: 24,
                        scrollback_rows: 24,
                        physical_top: 0,
                        scrollback_top: 0,
                        dpi: 96,
                        pixel_width: 800,
                        pixel_height: 480,
                        reverse_video: false,
                    },
                    tiered_scrollback_status: None,
                    dirty_lines: Vec::new(),
                    title: "shell".to_string(),
                    working_dir: None,
                    bonus_lines: SerializedLines::default(),
                    input_serial: None,
                    seqno: 1,
                }),
            )
            .await
        })
        .expect("render delta should apply");

        assert!(pane.is_alt_screen_active());
    }

    #[test]
    fn registration_slot_is_stable_and_rejects_concurrent_mux_owners() {
        let _scope = MuxTestScope::enter();
        let first_mux = Arc::new(Mux::new(None));
        let second_mux = Arc::new(Mux::new(None));
        let inner = test_client_inner(17);
        let pane = Arc::new(ClientPane::new(
            &inner,
            33,
            23,
            29,
            TerminalSize {
                cols: 80,
                rows: 24,
                pixel_width: 800,
                pixel_height: 480,
                dpi: 96,
            },
            "shell",
            false,
        ));

        assert!(
            Arc::ptr_eq(pane.mux_registration_slot(), pane.mux_registration_slot()),
            "a production ClientPane must expose one stable registration slot"
        );

        let pane_for_mux: Arc<dyn Pane> = pane.clone();
        first_mux
            .add_pane(&pane_for_mux)
            .expect("the first mux should bind the ClientPane");
        let slot_registration = pane
            .mux_registration_slot()
            .load()
            .expect("publication must populate the pane-owned slot");
        let registry_registration = first_mux
            .capture_pane_registration(&pane_for_mux)
            .expect("the mux registry must expose the same exact registration");
        assert!(
            slot_registration.same_registration(&registry_registration),
            "the production pane slot and mux registry must carry one generation authority"
        );

        let error = second_mux
            .add_pane(&pane_for_mux)
            .expect_err("one ClientPane object cannot be bound to two mux owners");
        assert!(
            error
                .to_string()
                .contains("already bound to a live or draining mux registration"),
            "unexpected dual-owner rejection: {:#}",
            error,
        );
    }

    #[test]
    fn unilateral_state_alert_is_forwarded_exactly_once() {
        let scope = MuxTestScope::enter();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);
        let observed = Arc::new(StdMutex::new(Vec::new()));
        let observed_for_subscriber = Arc::clone(&observed);
        mux.subscribe(move |notification| {
            if let MuxNotification::Alert { pane_id: 32, alert } = notification {
                observed_for_subscriber.lock().unwrap().push(alert);
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");

        let inner = test_client_inner(17);
        let pane = Arc::new(ClientPane::new(
            &inner,
            32,
            23,
            29,
            TerminalSize {
                cols: 80,
                rows: 24,
                pixel_width: 800,
                pixel_height: 480,
                dpi: 96,
            },
            "shell",
            false,
        ));
        let pane_for_mux: Arc<dyn Pane> = pane.clone();
        mux.add_pane(&pane_for_mux)
            .expect("unilateral-alert test pane should register");
        let registration = mux
            .capture_pane_registration(&pane_for_mux)
            .expect("unilateral-alert test pane should retain exact registration");
        let rpc = inner.client.rpc_scope();

        promise::spawn::block_on(async {
            pane.process_unilateral(
                &registration,
                &rpc,
                Pdu::NotifyAlert(NotifyAlert {
                    pane_id: 29,
                    alert: Alert::OutputSinceFocusLost,
                }),
            )
            .await?;
            pane.process_unilateral(
                &registration,
                &rpc,
                Pdu::NotifyAlert(NotifyAlert {
                    pane_id: 29,
                    alert: Alert::Progress(Progress::Percentage(64)),
                }),
            )
            .await
        })
        .expect("unilateral alerts should apply");

        assert!(*pane.unseen_output.lock());
        assert_eq!(*pane.progress.lock(), Progress::Percentage(64));
        assert_eq!(
            *observed.lock().unwrap(),
            vec![
                Alert::OutputSinceFocusLost,
                Alert::Progress(Progress::Percentage(64)),
            ],
            "state mutation and notification forwarding must not emit duplicate alerts"
        );
    }

    #[test]
    fn unilateral_rejects_registration_for_a_different_pane() {
        let scope = MuxTestScope::enter();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);
        let observed = Arc::new(StdMutex::new(Vec::new()));
        let observed_for_subscriber = Arc::clone(&observed);
        mux.subscribe(move |notification| {
            if let MuxNotification::Alert { pane_id, alert } = notification {
                observed_for_subscriber
                    .lock()
                    .unwrap()
                    .push((pane_id, alert));
            }
            true
        })
        .expect("wrong-registration test subscription should allocate an identifier");

        let inner = test_client_inner(17);
        let pane_a = test_client_pane(&inner, 34, 29);
        let pane_b = test_client_pane(&inner, 35, 30);
        let pane_a_for_mux: Arc<dyn Pane> = pane_a.clone();
        let pane_b_for_mux: Arc<dyn Pane> = pane_b.clone();
        mux.add_pane(&pane_a_for_mux)
            .expect("first wrong-registration test pane should register");
        mux.add_pane(&pane_b_for_mux)
            .expect("second wrong-registration test pane should register");
        let pane_b_registration = mux
            .capture_pane_registration(&pane_b_for_mux)
            .expect("second pane should retain exact registration");
        let rpc = inner.client.rpc_scope();

        promise::spawn::block_on(async {
            pane_a
                .process_unilateral(
                    &pane_b_registration,
                    &rpc,
                    Pdu::NotifyAlert(NotifyAlert {
                        pane_id: 29,
                        alert: Alert::OutputSinceFocusLost,
                    }),
                )
                .await
        })
        .expect("a wrong registration should be discarded without failing the reader");

        assert!(!*pane_a.unseen_output.lock());
        assert!(!*pane_b.unseen_output.lock());
        assert!(
            observed.lock().unwrap().is_empty(),
            "a registration for pane B must not authorize pane A or emit B-attributed alerts"
        );
    }

    #[test]
    fn unilateral_clipboard_callback_can_reenter_set_clipboard() {
        let scope = MuxTestScope::enter();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);
        let inner = test_client_inner(17);
        let pane = test_client_pane(&inner, 36, 31);
        let pane_for_mux: Arc<dyn Pane> = pane.clone();
        mux.add_pane(&pane_for_mux)
            .expect("clipboard reentrancy test pane should register");
        let registration = mux
            .capture_pane_registration(&pane_for_mux)
            .expect("clipboard reentrancy test pane should retain exact registration");
        let rpc = inner.client.rpc_scope();

        let callback_ran = Arc::new(AtomicBool::new(false));
        let clipboard: Arc<dyn Clipboard> = Arc::new(ReentrantClipboard {
            pane: Arc::downgrade(&pane),
            callback_ran: Arc::clone(&callback_ran),
        });
        pane.set_clipboard(&clipboard);

        promise::spawn::block_on(async {
            pane.process_unilateral(
                &registration,
                &rpc,
                Pdu::SetClipboard(SetClipboard {
                    pane_id: 31,
                    clipboard: Some("copied text".to_string()),
                    selection: wezterm_term::ClipboardSelection::Clipboard,
                }),
            )
            .await
        })
        .expect("clipboard callback should run outside the ClientPane clipboard mutex");

        assert!(
            callback_ran.load(Ordering::Acquire),
            "the clipboard callback should reenter set_clipboard without deadlocking"
        );
    }
}
