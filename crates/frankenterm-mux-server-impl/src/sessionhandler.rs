#![allow(clippy::future_not_send)]
use crate::PKI;
use anyhow::{Context, anyhow};
use codec::{
    ActivatePaneDirection, AdjustPaneSize, CODEC_VERSION, CreateFloatingPane, CycleStack,
    DecodedPdu, EraseScrollbackRequest, ErrorResponse, GetClientList, GetClientListResponse,
    GetCodecVersionResponse, GetImageCell, GetImageCellResponse, GetLines, GetLinesResponse,
    GetPaneDirection, GetPaneDirectionResponse, GetPaneRenderChanges, GetPaneRenderChangesResponse,
    GetPaneRenderableDimensions, GetPaneRenderableDimensionsResponse, GetSemanticZones,
    GetSemanticZonesResponse, GetTlsCredsResponse, InputSerial, KillPane, ListPanes,
    ListPanesResponse, ListPanesTabStackEntry, ListPanesTabStacks, ListPanesTabStacksResponse,
    LivenessResponse, MoveFloatingPane, MovePaneToNewTab, MovePaneToNewTabResponse, NotifyAlert,
    Pdu, Ping, Pong, RemoveFloatingPane, RenameWorkspace, Resize, SearchScrollbackRequest,
    SearchScrollbackResponse, SelectStackPane, SendKeyDown, SendKeyUp, SendMouseEvent, SendPaste,
    SetActiveWorkspace, SetClientId, SetFloatingPaneZ, SetFocusedPane, SetLayoutCycle, SetPalette,
    SetPaneZoomed, SetWindowWorkspace, SpawnResponse, SpawnV2, SplitPane, SwapToLayout,
    TabTitleChanged, ToggleFloatingPane, UnitResponse, UpdatePaneConstraints, WindowTitleChanged,
    WriteToPane,
};
use mux::client::ClientId;
use mux::domain::SplitSource;
use mux::pane::{CachePolicy, PaneId};
use mux::renderable::{PaneTieredScrollbackStatus, RenderableDimensions, StableCursorPosition};
use mux::{CurrentPane, Mux, PaneRegistrationHandle};
use promise::spawn::spawn_into_main_thread;
use std::collections::HashMap;
use std::ops::Deref;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Instant;
use termwiz::surface::SequenceNo;
use url::Url;
use wezterm_term::StableRowIndex;
use wezterm_term::terminal::Alert;

#[derive(Clone)]
pub struct PduSender {
    func: Arc<dyn Fn(DecodedPdu) -> anyhow::Result<()> + Send + Sync>,
}

impl PduSender {
    pub fn send(&self, pdu: DecodedPdu) -> anyhow::Result<()> {
        (self.func)(pdu)
    }

    pub fn new<T>(f: T) -> Self
    where
        T: Fn(DecodedPdu) -> anyhow::Result<()> + Send + Sync + 'static,
    {
        Self { func: Arc::new(f) }
    }
}

const SESSION_RETIRED: usize = 1usize << (usize::BITS - 1);
const SESSION_OPERATION_MASK: usize = SESSION_RETIRED - 1;

struct SessionIncarnation {
    operation_state: AtomicUsize,
}

impl SessionIncarnation {
    fn new() -> Self {
        Self {
            operation_state: AtomicUsize::new(0),
        }
    }

    fn try_acquire(self: &Arc<Self>) -> Option<SessionOperationLease> {
        let mut observed = self.operation_state.load(Ordering::Acquire);
        loop {
            if observed & SESSION_RETIRED != 0 {
                return None;
            }

            let active = observed & SESSION_OPERATION_MASK;
            if active == SESSION_OPERATION_MASK {
                return None;
            }

            match self.operation_state.compare_exchange_weak(
                observed,
                observed + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(SessionOperationLease {
                        incarnation: Arc::clone(self),
                    });
                }
                Err(current) => observed = current,
            }
        }
    }

    fn retire(&self) {
        self.operation_state
            .fetch_or(SESSION_RETIRED, Ordering::AcqRel);
    }

    fn release_operation(&self) {
        let mut observed = self.operation_state.load(Ordering::Acquire);
        loop {
            let active = observed & SESSION_OPERATION_MASK;
            if active == 0 {
                debug_assert!(
                    false,
                    "session operation lease released without a matching acquisition"
                );
                return;
            }

            match self.operation_state.compare_exchange_weak(
                observed,
                observed - 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(current) => observed = current,
            }
        }
    }
}

struct SessionOperationLease {
    incarnation: Arc<SessionIncarnation>,
}

impl Drop for SessionOperationLease {
    fn drop(&mut self) {
        self.incarnation.release_operation();
    }
}

/// Process-local authority for one exact mux-server connection.
///
/// Clones retain only the right to *attempt* an operation. Once the owning
/// [`SessionHandler`] retires the incarnation, deferred clones fail closed and
/// cannot redirect work through a replacement process-global mux.
#[derive(Clone)]
pub(crate) struct SessionAuthority {
    mux: Weak<Mux>,
    incarnation: Arc<SessionIncarnation>,
}

impl SessionAuthority {
    pub(crate) fn new(mux: &Arc<Mux>) -> Self {
        Self {
            mux: Arc::downgrade(mux),
            incarnation: Arc::new(SessionIncarnation::new()),
        }
    }

    fn acquire(&self) -> anyhow::Result<CurrentSession> {
        let operation = self
            .incarnation
            .try_acquire()
            .ok_or_else(|| anyhow!("mux server session is retired"))?;
        let mux = self
            .mux
            .upgrade()
            .ok_or_else(|| anyhow!("mux server session owner no longer exists"))?;
        Ok(CurrentSession {
            mux,
            _operation: operation,
        })
    }

    fn capture_current_pane(&self, pane_id: PaneId) -> anyhow::Result<PaneRegistrationHandle> {
        self.capture_current_pane_opt(pane_id)?
            .ok_or_else(|| anyhow!("no such pane {pane_id}"))
    }

    fn capture_current_pane_opt(
        &self,
        pane_id: PaneId,
    ) -> anyhow::Result<Option<PaneRegistrationHandle>> {
        let session = self.acquire()?;
        Ok(session.capture_current_pane(pane_id))
    }

    fn retire(&self) {
        self.incarnation.retire();
    }

    pub(crate) fn try_run<R>(&self, f: impl FnOnce() -> R) -> anyhow::Result<R> {
        let operation = self
            .incarnation
            .try_acquire()
            .ok_or_else(|| anyhow!("mux server session is retired"))?;
        let result = f();
        drop(operation);
        Ok(result)
    }
}

struct CurrentSession {
    mux: Arc<Mux>,
    _operation: SessionOperationLease,
}

impl Deref for CurrentSession {
    type Target = Mux;

    fn deref(&self) -> &Self::Target {
        &self.mux
    }
}

impl CurrentSession {
    fn mux(&self) -> &Arc<Mux> {
        &self.mux
    }
}

/// Unique strong owner for one mux-server connection.
///
/// Cloneable deferred work receives only [`SessionAuthority`], which contains
/// weak mux authority. This owner is therefore the sole connection-lifetime
/// strong reference and retires the incarnation before releasing the mux.
pub(crate) struct SessionOwner {
    mux: Arc<Mux>,
    authority: SessionAuthority,
}

impl SessionOwner {
    pub(crate) fn new(mux: Arc<Mux>) -> Self {
        let authority = SessionAuthority::new(&mux);
        Self { mux, authority }
    }

    pub(crate) fn mux(&self) -> &Arc<Mux> {
        &self.mux
    }

    pub(crate) fn authority(&self) -> SessionAuthority {
        self.authority.clone()
    }

    fn retire(&self) {
        self.authority.retire();
    }
}

impl Drop for SessionOwner {
    fn drop(&mut self) {
        self.retire();
    }
}

#[derive(Default, Debug)]
pub(crate) struct PerPane {
    cursor_position: StableCursorPosition,
    title: String,
    working_dir: Option<Url>,
    dimensions: RenderableDimensions,
    tiered_scrollback_status: Option<PaneTieredScrollbackStatus>,
    mouse_grabbed: bool,
    alt_screen_active: bool,
    sent_initial_palette: bool,
    seqno: SequenceNo,
    config_generation: usize,
    pub(crate) notifications: Vec<Alert>,
}

#[derive(Clone)]
struct TrackedPane {
    registration: Option<PaneRegistrationHandle>,
    state: Arc<Mutex<PerPane>>,
}

impl TrackedPane {
    fn exact(registration: PaneRegistrationHandle) -> Self {
        Self {
            registration: Some(registration),
            state: Arc::new(Mutex::new(PerPane::default())),
        }
    }
}

fn stable_row_offset(row: StableRowIndex, offset: usize) -> Option<StableRowIndex> {
    let offset = StableRowIndex::try_from(offset).ok()?;
    row.checked_add(offset)
}

fn stable_row_range_from_len(
    start: StableRowIndex,
    len: usize,
) -> Option<std::ops::Range<StableRowIndex>> {
    let end = stable_row_offset(start, len)?;
    Some(start..end)
}

#[cfg(test)]
fn stable_row_range_from_signed_len(
    start: StableRowIndex,
    len: StableRowIndex,
) -> Option<std::ops::Range<StableRowIndex>> {
    if len < 0 {
        return None;
    }

    let len = usize::try_from(len).ok()?;
    stable_row_range_from_len(start, len)
}

impl PerPane {
    fn compute_changes(
        &mut self,
        pane: &CurrentPane<'_>,
        force_with_input_serial: Option<InputSerial>,
    ) -> Option<GetPaneRenderChangesResponse> {
        let mut changed = false;
        let mouse_grabbed = pane.is_mouse_grabbed();
        if mouse_grabbed != self.mouse_grabbed {
            changed = true;
        }
        let alt_screen_active = pane.is_alt_screen_active();
        if alt_screen_active != self.alt_screen_active {
            changed = true;
        }

        let dims = pane.get_dimensions();
        if dims != self.dimensions {
            changed = true;
        }
        let tiered_scrollback_status = pane.get_tiered_scrollback_status();
        if tiered_scrollback_status != self.tiered_scrollback_status {
            changed = true;
        }

        let cursor_position = pane.get_cursor_position();
        if cursor_position != self.cursor_position {
            changed = true;
        }

        let title = pane.get_title();
        if title != self.title {
            changed = true;
        }

        let working_dir = pane.get_current_working_dir(CachePolicy::AllowStale);
        if working_dir != self.working_dir {
            changed = true;
        }

        let old_seqno = self.seqno;
        self.seqno = pane.get_current_seqno();
        let viewport_range = stable_row_range_from_len(dims.physical_top, dims.viewport_rows)
            .unwrap_or(dims.physical_top..dims.physical_top);
        let mut all_dirty_lines = pane.get_changed_since(viewport_range.clone(), old_seqno);
        if !all_dirty_lines.is_empty() {
            changed = true;
        }

        if !changed && !force_with_input_serial.is_some() {
            return None;
        }

        // Figure out what we're going to send as dirty lines vs bonus lines
        let (first_line, lines) = pane.get_lines(viewport_range);
        let mut bonus_lines = lines
            .into_iter()
            .enumerate()
            .filter_map(|(idx, mut line)| {
                let stable_row = stable_row_offset(first_line, idx)?;
                if all_dirty_lines.contains(stable_row) {
                    all_dirty_lines.remove(stable_row);
                    line.compress_for_scrollback();
                    Some((stable_row, line))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        // Always send the cursor's row, as that tends to the busiest and we don't
        // have a sequencing concept for our idea of the remote state.
        let (cursor_line_idx, lines) =
            pane.get_lines(cursor_position.y..cursor_position.y.saturating_add(1));
        if let Some(mut cursor_line) = lines.into_iter().next() {
            cursor_line.compress_for_scrollback();
            if bonus_lines
                .binary_search_by_key(&cursor_line_idx, |(stable_row, _)| *stable_row)
                .is_err()
            {
                bonus_lines.push((cursor_line_idx, cursor_line));
            }
        }

        self.cursor_position = cursor_position;
        self.title.clone_from(&title);
        self.working_dir.clone_from(&working_dir);
        self.dimensions = dims;
        self.tiered_scrollback_status = tiered_scrollback_status;
        self.mouse_grabbed = mouse_grabbed;
        self.alt_screen_active = alt_screen_active;

        let bonus_lines = bonus_lines.into();
        Some(GetPaneRenderChangesResponse {
            pane_id: pane.pane_id(),
            mouse_grabbed,
            alt_screen_active,
            dirty_lines: all_dirty_lines.iter().cloned().collect(),
            dimensions: dims,
            tiered_scrollback_status,
            cursor_position,
            title,
            bonus_lines,
            working_dir: working_dir.map(Into::into),
            input_serial: force_with_input_serial,
            seqno: self.seqno,
        })
    }
}

fn maybe_push_pane_changes(
    pane: &CurrentPane<'_>,
    sender: PduSender,
    per_pane: Arc<Mutex<PerPane>>,
) -> anyhow::Result<()> {
    let render_changes = {
        let mut per_pane = per_pane
            .lock()
            .map_err(|err| anyhow!("per-pane state lock poisoned: {err}"))?;
        per_pane.compute_changes(pane, None)
    };
    if let Some(resp) = render_changes {
        sender.send(DecodedPdu {
            pdu: Pdu::GetPaneRenderChangesResponse(resp),
            serial: 0,
        })?;
    }

    let notifications = {
        let mut per_pane = per_pane
            .lock()
            .map_err(|err| anyhow!("per-pane state lock poisoned: {err}"))?;
        let config = config::configuration();
        if per_pane.config_generation != config.generation() {
            per_pane.config_generation = config.generation();
            // If the config changed, it may have changed colors
            // in the palette that we need to push down, so we
            // synthesize a palette change notification to let
            // the client know
            per_pane.notifications.push(Alert::PaletteChanged);
            per_pane.sent_initial_palette = true;
        }

        if !per_pane.sent_initial_palette {
            per_pane.notifications.push(Alert::PaletteChanged);
            per_pane.sent_initial_palette = true;
        }
        std::mem::take(&mut per_pane.notifications)
    };

    for alert in notifications {
        match alert {
            Alert::PaletteChanged => {
                sender.send(DecodedPdu {
                    pdu: Pdu::SetPalette(SetPalette {
                        pane_id: pane.pane_id(),
                        palette: pane.palette(),
                    }),
                    serial: 0,
                })?;
            }
            alert => {
                sender.send(DecodedPdu {
                    pdu: Pdu::NotifyAlert(NotifyAlert {
                        pane_id: pane.pane_id(),
                        alert,
                    }),
                    serial: 0,
                })?;
            }
        }
    }
    Ok(())
}

fn session_mux(authority: &SessionAuthority) -> anyhow::Result<CurrentSession> {
    authority.acquire()
}

fn with_current_pane<R>(
    authority: &SessionAuthority,
    registration: &PaneRegistrationHandle,
    f: impl FnOnce(&CurrentPane<'_>) -> anyhow::Result<R>,
) -> anyhow::Result<R> {
    authority.try_run(|| {
        registration
            .try_with_current(|current| f(&current))
            .ok_or_else(|| {
                anyhow!(
                    "pane registration {} is no longer current",
                    registration.pane_id()
                )
            })?
    })?
}

fn unregister_owned_client(mux: &Mux, client_id: &Arc<ClientId>) {
    let _ = mux.unregister_client_if_same(client_id);
}

pub struct SessionHandler {
    to_write_tx: PduSender,
    owner: SessionOwner,
    per_pane: HashMap<PaneId, TrackedPane>,
    client_id: Option<Arc<ClientId>>,
    proxy_client_id: Option<ClientId>,
}

impl Drop for SessionHandler {
    fn drop(&mut self) {
        self.owner.retire();
        if let Some(client_id) = self.client_id.take() {
            unregister_owned_client(self.owner.mux(), &client_id);
        }
    }
}

impl SessionHandler {
    /// Construct a handler bound to one explicit mux incarnation.
    ///
    /// Production and fuzz callers must provide the owning mux rather than
    /// consulting the process-global singleton. Deferred work therefore cannot
    /// be redirected if another mux is installed while the session is alive.
    pub fn new_for_mux(to_write_tx: PduSender, mux: Arc<Mux>) -> Self {
        Self::new_for_session(to_write_tx, SessionOwner::new(mux))
    }

    pub(crate) fn new_for_session(to_write_tx: PduSender, owner: SessionOwner) -> Self {
        Self {
            to_write_tx,
            owner,
            per_pane: HashMap::new(),
            client_id: None,
            proxy_client_id: None,
        }
    }

    #[cfg(test)]
    fn new(to_write_tx: PduSender) -> Self {
        let mux_owner = Mux::try_get().unwrap_or_else(|| Arc::new(Mux::new(None)));
        Self::new_for_session(to_write_tx, SessionOwner::new(mux_owner))
    }

    fn per_pane_for_registration(
        &mut self,
        registration: &PaneRegistrationHandle,
    ) -> Arc<Mutex<PerPane>> {
        let pane_id = registration.pane_id();
        let tracked = self
            .per_pane
            .entry(pane_id)
            .and_modify(|tracked| {
                let is_same = tracked
                    .registration
                    .as_ref()
                    .is_some_and(|current| current.same_registration(registration));
                if !is_same {
                    *tracked = TrackedPane::exact(registration.clone());
                }
            })
            .or_insert_with(|| TrackedPane::exact(registration.clone()));
        Arc::clone(&tracked.state)
    }

    #[cfg(test)]
    fn per_pane(&mut self, pane_id: PaneId) -> Arc<Mutex<PerPane>> {
        let tracked = self.per_pane.entry(pane_id).or_insert_with(|| TrackedPane {
            registration: None,
            state: Arc::new(Mutex::new(PerPane::default())),
        });
        Arc::clone(&tracked.state)
    }

    /// Non-inserting accessor for cached per-pane state (ft-12e8l).
    ///
    /// Unlike [`per_pane`], this does NOT create an entry when the pane is
    /// untracked. Use this on push-side paths (e.g. late-arriving Alert
    /// notifications from mux) where silently re-creating an entry for a
    /// pane that was already removed via `PaneRemoved` produces a permanent
    /// map leak — no subsequent `PaneRemoved` ever fires for a dead pane.
    pub(crate) fn per_pane_if_present(&self, pane_id: PaneId) -> Option<Arc<Mutex<PerPane>>> {
        self.per_pane
            .get(&pane_id)
            .map(|tracked| Arc::clone(&tracked.state))
    }

    /// Remove cached per-pane state when a pane is destroyed.
    /// Prevents unbounded HashMap growth in long-lived sessions.
    pub(crate) fn remove_per_pane(&mut self, pane_id: PaneId) {
        let should_remove = self.per_pane.get(&pane_id).is_some_and(|tracked| {
            tracked
                .registration
                .as_ref()
                .is_none_or(|registration| registration.try_with_current(|_| ()).is_none())
        });
        if should_remove {
            self.per_pane.remove(&pane_id);
        }
    }

    fn remove_per_pane_if_same(&mut self, registration: &PaneRegistrationHandle) {
        let pane_id = registration.pane_id();
        let should_remove = self
            .per_pane
            .get(&pane_id)
            .and_then(|tracked| tracked.registration.as_ref())
            .is_some_and(|current| current.same_registration(registration));
        if should_remove {
            self.per_pane.remove(&pane_id);
        }
    }

    pub fn schedule_pane_push(&mut self, pane_id: PaneId) {
        let authority = self.owner.authority();
        let registration = match authority.capture_current_pane(pane_id) {
            Ok(registration) => registration,
            Err(err) => {
                log::debug!("skipping pane {pane_id} push: {err:#}");
                return;
            }
        };
        let sender = self.to_write_tx.clone();
        let per_pane = self.per_pane_for_registration(&registration);
        Self::schedule_pane_push_with_state(sender, authority, registration, per_pane);
    }

    /// Push cached pane changes only for panes this session already tracks.
    ///
    /// Mux notifications can arrive after `PaneRemoved`; those notifications
    /// must not recreate `per_pane` state for a pane that will never emit a
    /// later removal notification. Client request paths still use
    /// `schedule_pane_push`, which intentionally creates first-use state.
    pub(crate) fn schedule_tracked_pane_push(&self, pane_id: PaneId) {
        if let Some(tracked) = self.per_pane.get(&pane_id) {
            let Some(registration) = tracked.registration.clone() else {
                return;
            };
            let sender = self.to_write_tx.clone();
            let per_pane = Arc::clone(&tracked.state);
            Self::schedule_pane_push_with_state(
                sender,
                self.owner.authority(),
                registration,
                per_pane,
            );
        }
    }

    fn schedule_pane_push_with_state(
        sender: PduSender,
        authority: SessionAuthority,
        registration: PaneRegistrationHandle,
        per_pane: Arc<Mutex<PerPane>>,
    ) {
        spawn_into_main_thread(async move {
            authority.try_run(|| {
                registration
                    .try_with_current(|pane| maybe_push_pane_changes(&pane, sender, per_pane))
                    .ok_or_else(|| {
                        anyhow!(
                            "pane registration {} is no longer current",
                            registration.pane_id()
                        )
                    })?
            })??;
            Ok::<(), anyhow::Error>(())
        })
        .detach();
    }

    pub fn process_one(&mut self, decoded: DecodedPdu) {
        let start = Instant::now();
        let sender = self.to_write_tx.clone();
        let serial = decoded.serial;

        if let Some(client_id) = &self.client_id {
            if decoded.pdu.is_user_input() {
                match self.owner.authority().acquire() {
                    Ok(mux) => {
                        let _ = mux.client_had_input_if_same(client_id);
                    }
                    Err(err) => {
                        log::warn!("dropped client input activity marker: {err:#}");
                    }
                }
            }
        }

        let authority = self.owner.authority();
        let response_authority = authority.clone();
        let send_response = move |result: anyhow::Result<Pdu>| {
            let pdu = match result {
                Ok(pdu) => pdu,
                Err(err) => Pdu::ErrorResponse(ErrorResponse {
                    reason: format!("Error: {err:#}"),
                }),
            };
            log::trace!("{} processing time {:?}", serial, start.elapsed());
            let _ = response_authority.try_run(|| sender.send(DecodedPdu { pdu, serial }));
        };

        fn catch<F, SND>(f: F, send_response: SND)
        where
            F: FnOnce() -> anyhow::Result<Pdu>,
            SND: Fn(anyhow::Result<Pdu>),
        {
            send_response(f());
        }

        fn capture_pane_or_respond<SND>(
            authority: &SessionAuthority,
            pane_id: PaneId,
            send_response: &SND,
        ) -> Option<PaneRegistrationHandle>
        where
            SND: Fn(anyhow::Result<Pdu>),
        {
            match authority.capture_current_pane(pane_id) {
                Ok(registration) => Some(registration),
                Err(err) => {
                    send_response(Err(err));
                    None
                }
            }
        }

        fn capture_pane_or_respond_liveness<SND>(
            authority: &SessionAuthority,
            pane_id: PaneId,
            send_response: &SND,
        ) -> Option<PaneRegistrationHandle>
        where
            SND: Fn(anyhow::Result<Pdu>),
        {
            match authority.capture_current_pane_opt(pane_id) {
                Ok(Some(registration)) => Some(registration),
                Ok(None) => {
                    send_response(Ok(Pdu::LivenessResponse(LivenessResponse {
                        pane_id,
                        is_alive: false,
                    })));
                    None
                }
                Err(err) => {
                    send_response(Err(err));
                    None
                }
            }
        }

        match decoded.pdu {
            Pdu::Ping(Ping {}) => send_response(Ok(Pdu::Pong(Pong {}))),
            Pdu::SetWindowWorkspace(SetWindowWorkspace {
                window_id,
                workspace,
            }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = session_mux(&authority)?;
                            let mut window = mux
                                .get_window_mut(window_id)
                                .ok_or_else(|| anyhow!("window {} is invalid", window_id))?;
                            window.set_workspace(&workspace);
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    );
                })
                .detach();
            }
            Pdu::SetActiveWorkspace(SetActiveWorkspace { workspace }) => {
                let client_id = self.client_id.clone();
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let client_id = client_id.ok_or_else(|| {
                                anyhow!("set active workspace before SetClientId")
                            })?;
                            let mux = session_mux(&authority)?;
                            if !mux.set_active_workspace_for_client_if_same(&client_id, &workspace)
                            {
                                return Err(anyhow!("client registration is no longer current"));
                            }
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    );
                })
                .detach();
            }
            Pdu::SetClientId(SetClientId {
                mut client_id,
                is_proxy,
            }) => {
                if is_proxy {
                    if self.proxy_client_id.is_none() {
                        // Copy proxy identity, but don't assign it to the mux;
                        // we'll use it to annotate the actual clients own
                        // identity when they send it
                        self.proxy_client_id.replace(client_id);
                    }
                } else {
                    // If this session is a proxy, override the incoming id with
                    // the proxy information so that it is clear what is going
                    // on from the `wezterm cli list-clients` information
                    if let Some(proxy_id) = &self.proxy_client_id {
                        client_id.ssh_auth_sock.clone_from(&proxy_id.ssh_auth_sock);
                        // Note that this `via proxy pid` string is coupled
                        // with the logic in mux/src/ssh_agent
                        client_id.hostname =
                            format!("{} (via proxy pid {})", client_id.hostname, proxy_id.pid);
                    }

                    let client_id = Arc::new(client_id);
                    let mux = match session_mux(&authority) {
                        Ok(mux) => mux,
                        Err(err) => {
                            send_response(Err(err));
                            return;
                        }
                    };
                    let registration_is_current = self.client_id.as_ref().is_some_and(|current| {
                        current.as_ref() == client_id.as_ref()
                            && mux.client_registration_is_current(current)
                    });
                    if !registration_is_current {
                        if let Some(prior_client_id) = self.client_id.take() {
                            unregister_owned_client(&mux, &prior_client_id);
                        }
                        mux.register_client(Arc::clone(&client_id));
                        self.client_id = Some(client_id);
                    }
                }
                send_response(Ok(Pdu::UnitResponse(UnitResponse {})));
            }
            Pdu::SetFocusedPane(SetFocusedPane { pane_id }) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, pane_id, &send_response)
                else {
                    return;
                };
                let client_id = self.client_id.clone();
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            authority.try_run(|| {
                                registration
                                    .try_with_current(|current| {
                                        current.focus_for_client_if_same(client_id.as_ref())?;
                                        Ok(Pdu::UnitResponse(UnitResponse {}))
                                    })
                                    .ok_or_else(|| {
                                        anyhow!("pane registration {pane_id} is no longer current")
                                    })?
                            })?
                        },
                        send_response,
                    );
                })
                .detach();
            }
            Pdu::GetClientList(GetClientList) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = session_mux(&authority)?;
                            let clients = mux.iter_clients();
                            Ok(Pdu::GetClientListResponse(GetClientListResponse {
                                clients,
                            }))
                        },
                        send_response,
                    );
                })
                .detach();
            }
            Pdu::ListPanes(ListPanes {}) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = session_mux(&authority)?;
                            let mut tabs = vec![];
                            let mut tab_titles = vec![];
                            let mut window_titles = HashMap::new();
                            for window_id in mux.iter_windows() {
                                let window_snapshot = mux.get_window(window_id).map(|window| {
                                    (
                                        window.get_title().to_string(),
                                        window.get_workspace().to_string(),
                                        window.iter().cloned().collect::<Vec<_>>(),
                                    )
                                });
                                let Some((window_title, workspace, window_tabs)) = window_snapshot
                                else {
                                    log::warn!(
                                        "ListPanes skipped stale window id {} from iter_windows",
                                        window_id
                                    );
                                    continue;
                                };
                                window_titles.insert(window_id, window_title);
                                for tab in window_tabs {
                                    tabs.push(
                                        tab.codec_pane_tree_in_window(window_id, &workspace)?,
                                    );
                                    tab_titles.push(tab.get_title());
                                }
                            }
                            log::trace!(
                                "ListPanes snapshot has {} tab trees, {} tab titles, and {} windows",
                                tabs.len(),
                                tab_titles.len(),
                                window_titles.len()
                            );
                            Ok(Pdu::ListPanesResponse(ListPanesResponse {
                                tabs,
                                tab_titles,
                                window_titles,
                            }))
                        },
                        send_response,
                    );
                })
                .detach();
            }
            Pdu::ListPanesTabStacks(ListPanesTabStacks {}) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = session_mux(&authority)?;
                            let mut tab_stack_entries = vec![];
                            for window_id in mux.iter_windows() {
                                let Some(window) = mux.get_window(window_id) else {
                                    log::warn!(
                                        "ListPanesTabStacks skipped stale window id {} from iter_windows",
                                        window_id
                                    );
                                    continue;
                                };
                                tab_stack_entries.extend(window.tab_stack_entries().into_iter().map(
                                    |entry| ListPanesTabStackEntry {
                                        window_id,
                                        stack_id: entry.stack_id,
                                        tab_id: entry.tab_id,
                                        position: entry.position,
                                        is_visible: entry.is_visible,
                                    },
                                ));
                            }
                            log::trace!("ListPanesTabStacks {tab_stack_entries:#?}");
                            Ok(Pdu::ListPanesTabStacksResponse(
                                ListPanesTabStacksResponse { tab_stack_entries },
                            ))
                        },
                        send_response,
                    );
                })
                .detach();
            }

            Pdu::RenameWorkspace(RenameWorkspace {
                old_workspace,
                new_workspace,
            }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = session_mux(&authority)?;
                            mux.rename_workspace(&old_workspace, &new_workspace);
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    );
                })
                .detach();
            }

            Pdu::WriteToPane(WriteToPane { pane_id, data }) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, pane_id, &send_response)
                else {
                    return;
                };
                let sender = self.to_write_tx.clone();
                let per_pane = self.per_pane_for_registration(&registration);
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            with_current_pane(&authority, &registration, |pane| {
                                pane.write_all(&data)?;
                                maybe_push_pane_changes(pane, sender, per_pane)?;
                                Ok(Pdu::UnitResponse(UnitResponse {}))
                            })
                        },
                        send_response,
                    );
                })
                .detach();
            }
            Pdu::EraseScrollbackRequest(EraseScrollbackRequest {
                pane_id,
                erase_mode,
            }) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, pane_id, &send_response)
                else {
                    return;
                };
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            with_current_pane(&authority, &registration, |pane| {
                                pane.erase_scrollback(erase_mode);
                                Ok(Pdu::UnitResponse(UnitResponse {}))
                            })
                        },
                        send_response,
                    );
                })
                .detach();
            }
            Pdu::KillPane(KillPane { pane_id }) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, pane_id, &send_response)
                else {
                    return;
                };
                self.remove_per_pane_if_same(&registration);
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let retired = authority.try_run(|| registration.retire_if_current())?;
                            if !retired {
                                return Err(anyhow!(
                                    "pane registration {pane_id} is no longer current"
                                ));
                            }
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    );
                })
                .detach();
            }
            Pdu::SendPaste(SendPaste { pane_id, data }) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, pane_id, &send_response)
                else {
                    return;
                };
                let sender = self.to_write_tx.clone();
                let per_pane = self.per_pane_for_registration(&registration);
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            with_current_pane(&authority, &registration, |pane| {
                                pane.send_paste(&data)?;
                                maybe_push_pane_changes(pane, sender, per_pane)?;
                                Ok(Pdu::UnitResponse(UnitResponse {}))
                            })
                        },
                        send_response,
                    );
                })
                .detach();
            }

            Pdu::SearchScrollbackRequest(SearchScrollbackRequest {
                pane_id,
                pattern,
                range,
                limit,
            }) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, pane_id, &send_response)
                else {
                    return;
                };

                spawn_into_main_thread(async move {
                    promise::spawn::spawn(async move {
                        let result = async {
                            let session = authority.acquire()?;
                            let results = registration
                                .search_if_current(session.mux(), pattern, range, limit)
                                .await
                                .ok_or_else(|| {
                                    anyhow!("pane registration {pane_id} is no longer current")
                                })??;
                            Ok(Pdu::SearchScrollbackResponse(SearchScrollbackResponse {
                                results,
                            }))
                        }
                        .await;
                        send_response(result);
                    })
                    .detach();
                })
                .detach();
            }

            Pdu::SetPaneZoomed(SetPaneZoomed {
                containing_tab_id,
                pane_id,
                zoomed,
            }) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, pane_id, &send_response)
                else {
                    return;
                };
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            with_current_pane(&authority, &registration, |pane| {
                                pane.set_zoomed_in_tab(containing_tab_id, zoomed)?;
                                Ok(Pdu::UnitResponse(UnitResponse {}))
                            })
                        },
                        send_response,
                    );
                })
                .detach();
            }

            Pdu::GetPaneDirection(GetPaneDirection { pane_id, direction }) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, pane_id, &send_response)
                else {
                    return;
                };
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            with_current_pane(&authority, &registration, |pane| {
                                let pane_id = pane.pane_in_direction(direction)?;
                                Ok(Pdu::GetPaneDirectionResponse(GetPaneDirectionResponse {
                                    pane_id,
                                }))
                            })
                        },
                        send_response,
                    );
                })
                .detach();
            }

            Pdu::ActivatePaneDirection(ActivatePaneDirection { pane_id, direction }) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, pane_id, &send_response)
                else {
                    return;
                };
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            with_current_pane(&authority, &registration, |pane| {
                                pane.activate_pane_direction(direction)?;
                                Ok(Pdu::UnitResponse(UnitResponse {}))
                            })
                        },
                        send_response,
                    );
                })
                .detach();
            }

            Pdu::Resize(Resize {
                containing_tab_id,
                pane_id,
                size,
            }) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, pane_id, &send_response)
                else {
                    return;
                };
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            with_current_pane(&authority, &registration, |pane| {
                                pane.resize_in_tab(containing_tab_id, size)?;
                                Ok(Pdu::UnitResponse(UnitResponse {}))
                            })
                        },
                        send_response,
                    );
                })
                .detach();
            }

            Pdu::SendKeyDown(SendKeyDown {
                pane_id,
                event,
                input_serial,
            }) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, pane_id, &send_response)
                else {
                    return;
                };
                let sender = self.to_write_tx.clone();
                let per_pane = self.per_pane_for_registration(&registration);
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            with_current_pane(&authority, &registration, |pane| {
                                pane.key_down(event.key, event.modifiers)?;

                                // For a key press, always return the cursor
                                // position so predictive echo remains aligned.
                                let render_changes = {
                                    let mut per_pane = per_pane.lock().map_err(|err| {
                                        anyhow!("per-pane state lock poisoned: {err}")
                                    })?;
                                    per_pane.compute_changes(pane, Some(input_serial))
                                };
                                if let Some(resp) = render_changes {
                                    sender.send(DecodedPdu {
                                        pdu: Pdu::GetPaneRenderChangesResponse(resp),
                                        serial: 0,
                                    })?;
                                }
                                Ok(Pdu::UnitResponse(UnitResponse {}))
                            })
                        },
                        send_response,
                    );
                })
                .detach();
            }
            Pdu::SendKeyUp(SendKeyUp { pane_id, event }) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, pane_id, &send_response)
                else {
                    return;
                };
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            with_current_pane(&authority, &registration, |pane| {
                                pane.key_up(event.key, event.modifiers)?;
                                Ok(Pdu::UnitResponse(UnitResponse {}))
                            })
                        },
                        send_response,
                    );
                })
                .detach();
            }
            Pdu::SendMouseEvent(SendMouseEvent { pane_id, event }) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, pane_id, &send_response)
                else {
                    return;
                };
                let sender = self.to_write_tx.clone();
                let per_pane = self.per_pane_for_registration(&registration);
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            with_current_pane(&authority, &registration, |pane| {
                                pane.mouse_event(event)?;
                                maybe_push_pane_changes(pane, sender, per_pane)?;
                                Ok(Pdu::UnitResponse(UnitResponse {}))
                            })
                        },
                        send_response,
                    );
                })
                .detach();
            }

            Pdu::SpawnV2(spawn) => {
                let client_id = self.client_id.clone();
                spawn_into_main_thread(async move {
                    schedule_domain_spawn_v2(authority, spawn, send_response, client_id);
                })
                .detach();
            }

            Pdu::SplitPane(split) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, split.pane_id, &send_response)
                else {
                    return;
                };
                let move_registration = match split.move_pane_id {
                    Some(move_pane_id) => {
                        let Some(registration) =
                            capture_pane_or_respond(&authority, move_pane_id, &send_response)
                        else {
                            return;
                        };
                        Some(registration)
                    }
                    None => None,
                };
                let client_id = self.client_id.clone();
                spawn_into_main_thread(async move {
                    schedule_split_pane(
                        authority,
                        registration,
                        move_registration,
                        split,
                        send_response,
                        client_id,
                    );
                })
                .detach();
            }

            Pdu::MovePaneToNewTab(request) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, request.pane_id, &send_response)
                else {
                    return;
                };
                let client_id = self.client_id.clone();
                spawn_into_main_thread(async move {
                    schedule_move_pane(authority, registration, request, send_response, client_id);
                })
                .detach();
            }

            Pdu::GetPaneRenderableDimensions(GetPaneRenderableDimensions { pane_id }) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, pane_id, &send_response)
                else {
                    return;
                };
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            with_current_pane(&authority, &registration, |pane| {
                                let cursor_position = pane.get_cursor_position();
                                let dimensions = pane.get_dimensions();
                                Ok(Pdu::GetPaneRenderableDimensionsResponse(
                                    GetPaneRenderableDimensionsResponse {
                                        pane_id,
                                        cursor_position,
                                        dimensions,
                                        tiered_scrollback_status: pane
                                            .get_tiered_scrollback_status(),
                                    },
                                ))
                            })
                        },
                        send_response,
                    );
                })
                .detach();
            }

            Pdu::GetPaneRenderChanges(GetPaneRenderChanges { pane_id, .. }) => {
                let Some(registration) =
                    capture_pane_or_respond_liveness(&authority, pane_id, &send_response)
                else {
                    return;
                };
                let sender = self.to_write_tx.clone();
                let per_pane = self.per_pane_for_registration(&registration);
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let is_alive = authority
                                .try_run(|| {
                                    registration
                                        .try_with_current(|current| {
                                            maybe_push_pane_changes(&current, sender, per_pane)
                                        })
                                        .transpose()
                                })??
                                .is_some();
                            Ok(Pdu::LivenessResponse(LivenessResponse {
                                pane_id,
                                is_alive,
                            }))
                        },
                        send_response,
                    );
                })
                .detach();
            }

            Pdu::GetLines(GetLines { pane_id, lines }) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, pane_id, &send_response)
                else {
                    return;
                };
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            with_current_pane(&authority, &registration, |pane| {
                                let mut lines_and_indices = vec![];

                                for range in lines {
                                    let (first_row, lines) = pane.get_lines(range);
                                    for (idx, mut line) in lines.into_iter().enumerate() {
                                        let Some(stable_row) = stable_row_offset(first_row, idx)
                                        else {
                                            break;
                                        };
                                        line.compress_for_scrollback();
                                        lines_and_indices.push((stable_row, line));
                                    }
                                }
                                Ok(Pdu::GetLinesResponse(GetLinesResponse {
                                    pane_id,
                                    lines: lines_and_indices.into(),
                                }))
                            })
                        },
                        send_response,
                    );
                })
                .detach();
            }

            Pdu::GetSemanticZones(GetSemanticZones { pane_id }) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, pane_id, &send_response)
                else {
                    return;
                };
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            with_current_pane(&authority, &registration, |pane| {
                                let (zones, zone_texts, last_exit_code) =
                                    pane.semantic_snapshot()?;
                                Ok(Pdu::GetSemanticZonesResponse(GetSemanticZonesResponse {
                                    pane_id,
                                    zones,
                                    zone_texts,
                                    last_exit_code,
                                }))
                            })
                        },
                        send_response,
                    );
                })
                .detach();
            }

            Pdu::GetImageCell(GetImageCell {
                pane_id,
                line_idx,
                cell_idx,
                data_hash,
            }) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, pane_id, &send_response)
                else {
                    return;
                };
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            with_current_pane(&authority, &registration, |pane| {
                                let mut data = None;

                                let lines = match stable_row_range_from_len(line_idx, 1) {
                                    Some(line_range) => pane.get_lines(line_range).1,
                                    None => Vec::new(),
                                };
                                'found_data: for line in lines {
                                    if let Some(cell) = line.get_cell(cell_idx) {
                                        if let Some(images) = cell.attrs().images() {
                                            for im in images {
                                                if im.image_data().hash() == data_hash {
                                                    data.replace(im.image_data().clone());
                                                    break 'found_data;
                                                }
                                            }
                                        }
                                    }
                                }
                                Ok(Pdu::GetImageCellResponse(GetImageCellResponse {
                                    pane_id,
                                    data,
                                }))
                            })
                        },
                        send_response,
                    );
                })
                .detach();
            }

            Pdu::GetCodecVersion(_) => {
                match std::env::current_exe().context("resolving current_exe") {
                    Err(err) => send_response(Err(err)),
                    Ok(executable_path) => {
                        send_response(Ok(Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                            codec_vers: CODEC_VERSION,
                            version_string: config::wezterm_version().to_owned(),
                            executable_path,
                            config_file_path: std::env::var_os("WEZTERM_CONFIG_FILE")
                                .map(Into::into),
                            min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                        })));
                    }
                }
            }

            Pdu::GetTlsCreds(_) => {
                catch(
                    move || {
                        let client_cert_pem = PKI.generate_client_cert()?;
                        let ca_cert_pem = PKI.ca_pem_string()?;
                        Ok(Pdu::GetTlsCredsResponse(GetTlsCredsResponse {
                            client_cert_pem,
                            ca_cert_pem,
                        }))
                    },
                    send_response,
                );
            }
            Pdu::WindowTitleChanged(WindowTitleChanged { window_id, title }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = session_mux(&authority)?;
                            if mux.get_window(window_id).is_none() {
                                return Err(anyhow!("no such window {window_id}"));
                            }
                            mux.set_window_title(window_id, &title);

                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    );
                })
                .detach();
            }
            Pdu::TabTitleChanged(TabTitleChanged { tab_id, title }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = session_mux(&authority)?;
                            if mux.get_tab(tab_id).is_none() {
                                return Err(anyhow!("no such tab {tab_id}"));
                            }
                            mux.set_tab_title(tab_id, &title);

                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    );
                })
                .detach();
            }
            Pdu::SetPalette(SetPalette { pane_id, palette }) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, pane_id, &send_response)
                else {
                    return;
                };
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            with_current_pane(&authority, &registration, |pane| {
                                pane.set_client_palette(palette);
                                Ok(Pdu::UnitResponse(UnitResponse {}))
                            })
                        },
                        send_response,
                    );
                })
                .detach();
            }

            Pdu::AdjustPaneSize(AdjustPaneSize {
                pane_id,
                direction,
                amount,
            }) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, pane_id, &send_response)
                else {
                    return;
                };
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            with_current_pane(&authority, &registration, |pane| {
                                pane.adjust_pane_size(direction, amount)?;
                                Ok(Pdu::UnitResponse(UnitResponse {}))
                            })
                        },
                        send_response,
                    );
                })
                .detach();
            }

            Pdu::CreateFloatingPane(CreateFloatingPane {
                tab_id,
                pane_id,
                rect,
            }) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, pane_id, &send_response)
                else {
                    return;
                };
                spawn_into_main_thread({
                    let send_response = send_response.clone();
                    async move {
                        catch(
                            move || {
                                with_current_pane(&authority, &registration, |pane| {
                                    pane.create_floating_pane(tab_id, rect)?;
                                    Ok(Pdu::UnitResponse(UnitResponse {}))
                                })
                            },
                            send_response,
                        );
                    }
                })
                .detach();
            }
            Pdu::MoveFloatingPane(MoveFloatingPane { pane_id, rect }) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, pane_id, &send_response)
                else {
                    return;
                };
                spawn_into_main_thread({
                    let send_response = send_response.clone();
                    async move {
                        catch(
                            move || {
                                with_current_pane(&authority, &registration, |pane| {
                                    pane.move_floating_pane(rect)?;
                                    Ok(Pdu::UnitResponse(UnitResponse {}))
                                })
                            },
                            send_response,
                        );
                    }
                })
                .detach();
            }
            Pdu::SetFloatingPaneZ(SetFloatingPaneZ { pane_id, z_order }) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, pane_id, &send_response)
                else {
                    return;
                };
                spawn_into_main_thread({
                    let send_response = send_response.clone();
                    async move {
                        catch(
                            move || {
                                with_current_pane(&authority, &registration, |pane| {
                                    pane.set_floating_pane_z_order(z_order)?;
                                    Ok(Pdu::UnitResponse(UnitResponse {}))
                                })
                            },
                            send_response,
                        );
                    }
                })
                .detach();
            }
            Pdu::ToggleFloatingPane(ToggleFloatingPane { pane_id, visible }) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, pane_id, &send_response)
                else {
                    return;
                };
                spawn_into_main_thread({
                    let send_response = send_response.clone();
                    async move {
                        catch(
                            move || {
                                with_current_pane(&authority, &registration, |pane| {
                                    pane.set_floating_pane_visible(visible)?;
                                    Ok(Pdu::UnitResponse(UnitResponse {}))
                                })
                            },
                            send_response,
                        );
                    }
                })
                .detach();
            }
            Pdu::RemoveFloatingPane(RemoveFloatingPane { pane_id }) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, pane_id, &send_response)
                else {
                    return;
                };
                spawn_into_main_thread({
                    let send_response = send_response.clone();
                    async move {
                        catch(
                            move || {
                                with_current_pane(&authority, &registration, |pane| {
                                    pane.remove_floating_pane()?;
                                    Ok(Pdu::UnitResponse(UnitResponse {}))
                                })
                            },
                            send_response,
                        );
                    }
                })
                .detach();
            }
            Pdu::SwapToLayout(SwapToLayout {
                tab_id,
                layout_index,
            }) => {
                spawn_into_main_thread({
                    let send_response = send_response.clone();
                    async move {
                        catch(
                            move || {
                                let mux = session_mux(&authority)?;
                                let tab = mux
                                    .get_tab(tab_id)
                                    .ok_or_else(|| anyhow!("no such tab {}", tab_id))?;
                                tab.swap_to_layout_index(layout_index);
                                Ok(Pdu::UnitResponse(UnitResponse {}))
                            },
                            send_response,
                        );
                    }
                })
                .detach();
            }
            Pdu::SetLayoutCycle(SetLayoutCycle {
                tab_id,
                layout_names,
            }) => {
                spawn_into_main_thread({
                    let send_response = send_response.clone();
                    async move {
                        catch(
                            move || {
                                let mux = session_mux(&authority)?;
                                let tab = mux
                                    .get_tab(tab_id)
                                    .ok_or_else(|| anyhow!("no such tab {}", tab_id))?;
                                // Build layout cycle from named presets
                                let mut layouts = Vec::new();
                                for name in &layout_names {
                                    let layout = match name.as_str() {
                                        "grid-4" => mux::layout::grid_4(),
                                        "main-side" => mux::layout::main_side(),
                                        "stacked" => mux::layout::stacked(),
                                        "main-bottom" => mux::layout::main_bottom(),
                                        other => {
                                            return Err(anyhow!("unknown layout preset: {other}"));
                                        }
                                    };
                                    layouts.push(layout);
                                }
                                if layouts.is_empty() {
                                    return Err(anyhow!(
                                        "layout cycle must have at least one layout"
                                    ));
                                }
                                tab.set_layout_cycle(mux::layout::LayoutCycle::new(layouts));
                                Ok(Pdu::UnitResponse(UnitResponse {}))
                            },
                            send_response,
                        );
                    }
                })
                .detach();
            }
            Pdu::CycleStack(CycleStack {
                tab_id,
                slot_index,
                forward,
            }) => {
                spawn_into_main_thread({
                    let send_response = send_response.clone();
                    async move {
                        catch(
                            move || {
                                let mux = session_mux(&authority)?;
                                let tab = mux
                                    .get_tab(tab_id)
                                    .ok_or_else(|| anyhow!("no such tab {}", tab_id))?;
                                if forward {
                                    tab.cycle_stack(slot_index);
                                } else {
                                    tab.cycle_stack_backward(slot_index);
                                }
                                Ok(Pdu::UnitResponse(UnitResponse {}))
                            },
                            send_response,
                        );
                    }
                })
                .detach();
            }
            Pdu::SelectStackPane(SelectStackPane {
                tab_id,
                slot_index,
                pane_index,
            }) => {
                spawn_into_main_thread({
                    let send_response = send_response.clone();
                    async move {
                        catch(
                            move || {
                                let mux = session_mux(&authority)?;
                                let tab = mux
                                    .get_tab(tab_id)
                                    .ok_or_else(|| anyhow!("no such tab {}", tab_id))?;
                                tab.select_stack_pane(slot_index, pane_index)
                                    .ok_or_else(|| {
                                        anyhow!(
                                            "stack slot {} or pane index {} not found in tab {}",
                                            slot_index,
                                            pane_index,
                                            tab_id
                                        )
                                    })?;
                                Ok(Pdu::UnitResponse(UnitResponse {}))
                            },
                            send_response,
                        );
                    }
                })
                .detach();
            }
            Pdu::UpdatePaneConstraints(UpdatePaneConstraints {
                pane_id,
                min_width,
                max_width,
                min_height,
                max_height,
            }) => {
                let Some(registration) =
                    capture_pane_or_respond(&authority, pane_id, &send_response)
                else {
                    return;
                };
                spawn_into_main_thread({
                    let send_response = send_response.clone();
                    async move {
                        catch(
                            move || {
                                with_current_pane(&authority, &registration, |pane| {
                                    pane.update_pane_constraints(
                                        min_width, max_width, min_height, max_height,
                                    )?;
                                    Ok(Pdu::UnitResponse(UnitResponse {}))
                                })
                            },
                            send_response,
                        );
                    }
                })
                .detach();
            }
            Pdu::RenderApplicationResult(_) => {
                send_response(Err(anyhow!(
                    "render-application settlement received before the live delivery \
                     coordinator was activated for this connection"
                )));
            }
            Pdu::Invalid { .. } => send_response(Err(anyhow!("invalid PDU {:?}", decoded.pdu))),
            Pdu::Pong { .. }
            | Pdu::ListPanesResponse { .. }
            | Pdu::ListPanesTabStacksResponse { .. }
            | Pdu::SetClipboard { .. }
            | Pdu::NotifyAlert { .. }
            | Pdu::SpawnResponse { .. }
            | Pdu::GetPaneRenderChangesResponse { .. }
            | Pdu::RenderApplicationUpdate { .. }
            | Pdu::UnitResponse { .. }
            | Pdu::LivenessResponse { .. }
            | Pdu::GetPaneDirectionResponse { .. }
            | Pdu::SearchScrollbackResponse { .. }
            | Pdu::GetLinesResponse { .. }
            | Pdu::GetSemanticZonesResponse { .. }
            | Pdu::GetCodecVersionResponse { .. }
            | Pdu::WindowWorkspaceChanged { .. }
            | Pdu::GetTlsCredsResponse { .. }
            | Pdu::GetClientListResponse { .. }
            | Pdu::PaneRemoved { .. }
            | Pdu::PaneFocused { .. }
            | Pdu::TabResized { .. }
            | Pdu::GetImageCellResponse { .. }
            | Pdu::MovePaneToNewTabResponse { .. }
            | Pdu::TabAddedToWindow { .. }
            | Pdu::GetPaneRenderableDimensionsResponse { .. }
            | Pdu::ErrorResponse { .. } => {
                send_response(Err(anyhow!("expected a request, got {:?}", decoded.pdu)));
            }
        }
    }
}

// Dancing around a little bit here; we can't directly spawn_into_main_thread the domain_spawn
// function below because the compiler thinks that all of its locals then need to be Send.
// We need to shimmy through this helper to break that aspect of the compiler flow
// analysis and allow things to compile.
fn schedule_domain_spawn_v2<SND>(
    authority: SessionAuthority,
    spawn: SpawnV2,
    send_response: SND,
    client_id: Option<Arc<ClientId>>,
) where
    SND: Fn(anyhow::Result<Pdu>) + 'static,
{
    promise::spawn::spawn(async move {
        send_response(domain_spawn_v2(authority, spawn, client_id).await);
    })
    .detach();
}

fn schedule_split_pane<SND>(
    authority: SessionAuthority,
    registration: PaneRegistrationHandle,
    move_registration: Option<PaneRegistrationHandle>,
    split: SplitPane,
    send_response: SND,
    client_id: Option<Arc<ClientId>>,
) where
    SND: Fn(anyhow::Result<Pdu>) + 'static,
{
    promise::spawn::spawn(async move {
        send_response(
            split_pane(authority, registration, move_registration, split, client_id).await,
        );
    })
    .detach();
}

async fn split_pane(
    authority: SessionAuthority,
    registration: PaneRegistrationHandle,
    move_registration: Option<PaneRegistrationHandle>,
    split: SplitPane,
    client_id: Option<Arc<ClientId>>,
) -> anyhow::Result<Pdu> {
    let session = authority.acquire()?;

    let source = if let Some(move_pane_id) = split.move_pane_id {
        SplitSource::MovePane(move_pane_id)
    } else {
        SplitSource::Spawn {
            command: split.command,
            command_dir: split.command_dir,
        }
    };

    let (pane_id, size, window_id, tab_id) = registration
        .split_if_current(
            session.mux(),
            move_registration.as_ref(),
            split.split_request,
            source,
            split.domain,
            client_id,
        )
        .await
        .ok_or_else(|| {
            anyhow!(
                "pane registration {} is no longer current",
                registration.pane_id()
            )
        })??;

    Ok::<Pdu, anyhow::Error>(Pdu::SpawnResponse(SpawnResponse {
        pane_id,
        tab_id,
        window_id,
        size,
    }))
}

async fn domain_spawn_v2(
    authority: SessionAuthority,
    spawn: SpawnV2,
    client_id: Option<Arc<ClientId>>,
) -> anyhow::Result<Pdu> {
    let mux = session_mux(&authority)?;

    let (tab, pane, window_id) = mux
        .mux()
        .spawn_tab_or_window(
            spawn.window_id,
            spawn.domain,
            spawn.command,
            spawn.command_dir,
            spawn.size,
            None, // optional current pane_id
            spawn.workspace,
            None, // optional gui window position
            client_id,
        )
        .await?;

    Ok::<Pdu, anyhow::Error>(Pdu::SpawnResponse(SpawnResponse {
        pane_id: pane.pane_id(),
        tab_id: tab.tab_id(),
        window_id,
        size: tab.get_size(),
    }))
}

fn schedule_move_pane<SND>(
    authority: SessionAuthority,
    registration: PaneRegistrationHandle,
    request: MovePaneToNewTab,
    send_response: SND,
    client_id: Option<Arc<ClientId>>,
) where
    SND: Fn(anyhow::Result<Pdu>) + 'static,
{
    promise::spawn::spawn(async move {
        send_response(move_pane(authority, registration, request, client_id).await);
    })
    .detach();
}

async fn move_pane(
    authority: SessionAuthority,
    registration: PaneRegistrationHandle,
    request: MovePaneToNewTab,
    client_id: Option<Arc<ClientId>>,
) -> anyhow::Result<Pdu> {
    let session = authority.acquire()?;
    let (tab_id, window_id) = registration
        .move_to_new_tab_if_current(
            session.mux(),
            request.window_id,
            request.workspace_for_new_window,
            client_id,
        )
        .await
        .ok_or_else(|| {
            anyhow!(
                "pane registration {} is no longer current",
                registration.pane_id()
            )
        })??;

    Ok::<Pdu, anyhow::Error>(Pdu::MovePaneToNewTabResponse(MovePaneToNewTabResponse {
        tab_id,
        window_id,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::keyassignment::PaneDirection;
    use mux::domain::DomainId;
    use mux::pane::{CachePolicy, ForEachPaneLogicalLine, LogicalLine, Pane, WithPaneLines};
    use parking_lot::{MappedMutexGuard, Mutex as ParkingMutex, MutexGuard as ParkingMutexGuard};
    use promise::spawn::SimpleExecutor;
    use proptest::prelude::*;
    use rangeset::RangeSet;
    use std::collections::{HashMap, HashSet};
    use std::ops::Range;
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;
    use termwiz::surface::Line;
    use wezterm_term::color::ColorPalette;
    use wezterm_term::{KeyCode, KeyModifiers, MouseEvent, StableRowIndex, TerminalSize};

    struct ScopedMux(Option<Arc<Mux>>);

    impl ScopedMux {
        fn install(mux: &Arc<Mux>) -> Self {
            let prior = Mux::try_get();
            Mux::set_mux(mux);
            Self(prior)
        }

        fn shutdown_current() -> Self {
            let prior = Mux::try_get();
            Mux::shutdown();
            Self(prior)
        }
    }

    impl Drop for ScopedMux {
        fn drop(&mut self) {
            if let Some(prior) = self.0.take() {
                Mux::set_mux(&prior);
            } else {
                Mux::shutdown();
            }
        }
    }

    #[derive(Clone)]
    struct FakePaneState {
        cursor_position: StableCursorPosition,
        dimensions: RenderableDimensions,
        tiered_scrollback_status: Option<PaneTieredScrollbackStatus>,
        title: String,
        working_dir: Option<Url>,
        alt_screen_active: bool,
        seqno: SequenceNo,
        lines: Vec<Line>,
    }

    struct FakePane {
        pane_id: PaneId,
        state: Mutex<FakePaneState>,
        changed_lines: Mutex<RangeSet<StableRowIndex>>,
        callback_probe: Option<Arc<dyn Fn() + Send + Sync>>,
        // Writer sink for the Pane::writer() trait obligation. FakePane
        // has no PTY; bytes written here are discarded. Keeping a real
        // writable buffer behind a parking_lot mutex (rather than
        // panicking via unimplemented!) means tests that exercise
        // writer() — e.g., paste handlers, scripted-input drivers —
        // don't crash the test binary on a code path that's
        // semantically a no-op for a fake pane. (br-ft-35yac.3)
        writer_sink: ParkingMutex<std::io::Sink>,
        mux_registration: Arc<mux::PaneRegistrationSlot>,
    }

    impl FakePane {
        fn new(tiered_scrollback_status: Option<PaneTieredScrollbackStatus>) -> Self {
            Self::new_with_id(77, tiered_scrollback_status)
        }

        fn new_with_id(
            pane_id: PaneId,
            tiered_scrollback_status: Option<PaneTieredScrollbackStatus>,
        ) -> Self {
            Self {
                pane_id,
                callback_probe: None,
                writer_sink: ParkingMutex::new(std::io::sink()),
                mux_registration: Arc::new(mux::PaneRegistrationSlot::default()),
                changed_lines: Mutex::new(RangeSet::new()),
                state: Mutex::new(FakePaneState {
                    cursor_position: StableCursorPosition {
                        x: 4,
                        y: 0,
                        ..Default::default()
                    },
                    dimensions: RenderableDimensions {
                        cols: 80,
                        viewport_rows: 2,
                        scrollback_rows: 2,
                        physical_top: 0,
                        scrollback_top: 0,
                        dpi: 96,
                        pixel_width: 640,
                        pixel_height: 480,
                        reverse_video: false,
                    },
                    tiered_scrollback_status,
                    title: "tiered-pane".to_string(),
                    working_dir: Url::parse("file:///tmp/tiered-pane").ok(),
                    alt_screen_active: false,
                    seqno: 11,
                    lines: vec![
                        Line::from_text("alpha", &Default::default(), 1, None),
                        Line::from_text("beta", &Default::default(), 1, None),
                    ],
                }),
            }
        }

        fn new_with_callback_probe(
            pane_id: PaneId,
            callback_probe: Arc<dyn Fn() + Send + Sync>,
        ) -> Self {
            let mut pane = Self::new_with_id(pane_id, None);
            pane.callback_probe = Some(callback_probe);
            pane
        }

        fn set_tiered_scrollback_status(&self, status: Option<PaneTieredScrollbackStatus>) {
            self.state.lock().unwrap().tiered_scrollback_status = status;
        }

        fn set_changed_line(&self, stable_row: StableRowIndex) {
            self.changed_lines.lock().unwrap().add(stable_row);
        }
    }

    impl Pane for FakePane {
        fn pane_id(&self) -> PaneId {
            self.pane_id
        }

        fn mux_registration_slot(&self) -> &Arc<mux::PaneRegistrationSlot> {
            &self.mux_registration
        }

        fn get_cursor_position(&self) -> StableCursorPosition {
            self.state.lock().unwrap().cursor_position
        }

        fn get_current_seqno(&self) -> SequenceNo {
            self.state.lock().unwrap().seqno
        }

        fn get_changed_since(
            &self,
            _lines: Range<StableRowIndex>,
            _seqno: SequenceNo,
        ) -> RangeSet<StableRowIndex> {
            self.changed_lines.lock().unwrap().clone()
        }

        fn get_lines(&self, lines: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
            let state = self.state.lock().unwrap();
            (
                lines.start,
                state
                    .lines
                    .iter()
                    .skip(lines.start as usize)
                    .take((lines.end - lines.start) as usize)
                    .cloned()
                    .collect(),
            )
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

        fn get_logical_lines(&self, lines: Range<StableRowIndex>) -> Vec<LogicalLine> {
            mux::pane::impl_get_logical_lines_via_get_lines(self, lines)
        }

        fn get_dimensions(&self) -> RenderableDimensions {
            if let Some(probe) = &self.callback_probe {
                probe();
            }
            self.state.lock().unwrap().dimensions
        }

        fn get_tiered_scrollback_status(&self) -> Option<PaneTieredScrollbackStatus> {
            self.state.lock().unwrap().tiered_scrollback_status
        }

        fn get_title(&self) -> String {
            self.state.lock().unwrap().title.clone()
        }

        fn send_paste(&self, _text: &str) -> anyhow::Result<()> {
            Ok(())
        }

        fn reader(&self) -> anyhow::Result<Option<Box<dyn std::io::Read + Send>>> {
            Ok(None)
        }

        fn writer(&self) -> MappedMutexGuard<'_, dyn std::io::Write> {
            // Discarding sink: FakePane has no underlying PTY, so any
            // bytes written are dropped on the floor. This replaces a
            // prior `unimplemented!()` panic-bomb (br-ft-35yac.3) so
            // test code that walks Pane::writer() without first
            // checking the pane's class doesn't crash the test binary.
            ParkingMutexGuard::map(self.writer_sink.lock(), |sink| {
                let writer: &mut dyn std::io::Write = sink;
                writer
            })
        }

        fn resize(&self, size: TerminalSize) -> anyhow::Result<()> {
            let mut state = self.state.lock().unwrap();
            state.dimensions.cols = size.cols;
            state.dimensions.viewport_rows = size.rows;
            state.dimensions.scrollback_rows = size.rows;
            state.dimensions.dpi = size.dpi;
            state.dimensions.pixel_width = size.pixel_width;
            state.dimensions.pixel_height = size.pixel_height;
            Ok(())
        }

        fn key_down(&self, _key: KeyCode, _mods: KeyModifiers) -> anyhow::Result<()> {
            Ok(())
        }

        fn key_up(&self, _key: KeyCode, _mods: KeyModifiers) -> anyhow::Result<()> {
            Ok(())
        }

        fn mouse_event(&self, _event: MouseEvent) -> anyhow::Result<()> {
            Ok(())
        }

        fn is_dead(&self) -> bool {
            false
        }

        fn palette(&self) -> ColorPalette {
            ColorPalette::default()
        }

        fn domain_id(&self) -> DomainId {
            DomainId::default()
        }

        fn is_mouse_grabbed(&self) -> bool {
            false
        }

        fn is_alt_screen_active(&self) -> bool {
            self.state.lock().unwrap().alt_screen_active
        }

        fn get_current_working_dir(&self, _policy: CachePolicy) -> Option<Url> {
            self.state.lock().unwrap().working_dir.clone()
        }
    }

    fn sample_tiered_scrollback_status(cold_spill_lines_total: u64) -> PaneTieredScrollbackStatus {
        PaneTieredScrollbackStatus {
            tiering_enabled: true,
            configured_scrollback_rows: 10_000,
            configured_hot_lines: 512,
            configured_warm_max_bytes: 8 * 1024,
            visible_rows: 2,
            in_memory_scrollback_rows: 6,
            warm_resident_lines: 4,
            warm_resident_bytes: 512,
            warm_spill_lines_total: cold_spill_lines_total + 4,
            warm_spill_bytes_total: 4096,
            cold_spill_lines_total,
            cold_spill_bytes_total: cold_spill_lines_total * 64,
            cold_sink_retained_lines: 0,
            cold_sink_retained_bytes: 0,
            cold_worker_peak_backlog_depth: 3,
            cold_worker_completion_throughput_lines_per_sec: 256,
            cold_worker_completed_lines_total: cold_spill_lines_total,
            cold_worker_completed_batches_total: 2,
            cold_worker_cancellation_count: 0,
        }
    }

    /// Creates a PduSender that captures all sent PDUs into a shared Vec.
    fn capturing_sender() -> (PduSender, Arc<Mutex<Vec<DecodedPdu>>>) {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = Arc::clone(&captured);
        let sender = PduSender::new(move |pdu| {
            captured_clone.lock().unwrap().push(pdu);
            Ok(())
        });
        (sender, captured)
    }

    fn register_test_pane(pane: &Arc<dyn Pane>) -> (Arc<Mux>, mux::PaneRegistrationHandle) {
        let mux = Arc::new(Mux::new(None));
        mux.add_pane(pane).expect("test pane registration");
        let registration = mux
            .capture_pane_registration(pane)
            .expect("test pane should yield an exact handle");
        (mux, registration)
    }

    /// Extract the single response PDU from the captured list.
    fn take_response(captured: &Arc<Mutex<Vec<DecodedPdu>>>) -> DecodedPdu {
        let mut v = captured.lock().unwrap();
        assert_eq!(v.len(), 1, "expected exactly one response PDU");
        v.remove(0)
    }

    fn tick_until_response(
        executor: &SimpleExecutor,
        captured: &Arc<Mutex<Vec<DecodedPdu>>>,
        expected: usize,
    ) {
        for _ in 0..16 {
            if captured.lock().unwrap().len() >= expected {
                return;
            }
            executor.tick().unwrap();
        }

        let observed = captured.lock().unwrap().len();
        panic!("timed out waiting for {expected} response PDUs; saw {observed}");
    }

    fn test_tab_size() -> TerminalSize {
        TerminalSize {
            rows: 40,
            cols: 160,
            pixel_width: 1600,
            pixel_height: 1000,
            dpi: 96,
        }
    }

    fn install_tab_with_window(
        tab: &Arc<mux::tab::Tab>,
        extra_panes: &[Arc<dyn Pane>],
    ) -> (Arc<Mux>, ScopedMux) {
        let mux = Arc::new(Mux::new(None));
        let guard = ScopedMux::install(&mux);
        let window = mux.new_empty_window(None, None);
        let window_id = *window;
        mux.add_tab_and_active_pane(tab).unwrap();
        for pane in extra_panes {
            mux.add_pane(pane).unwrap();
        }
        mux.add_tab_to_window(tab, window_id).unwrap();
        drop(window);
        (mux, guard)
    }

    #[test]
    fn stable_row_range_helpers_reject_unrepresentable_spans() {
        assert_eq!(
            stable_row_range_from_len(StableRowIndex::MAX - 1, 1),
            Some((StableRowIndex::MAX - 1)..StableRowIndex::MAX)
        );
        assert_eq!(stable_row_range_from_len(StableRowIndex::MAX, 1), None);
        assert_eq!(
            stable_row_range_from_signed_len(StableRowIndex::MAX - 1, 2),
            None
        );
        assert_eq!(stable_row_range_from_signed_len(10, -1), None);
    }

    #[test]
    fn session_handler_new_has_empty_state() {
        let (sender, _captured) = capturing_sender();
        let handler = SessionHandler::new(sender);
        assert!(handler.client_id.is_none());
        assert!(handler.proxy_client_id.is_none());
        assert!(handler.per_pane.is_empty());
    }

    #[test]
    fn per_pane_creates_and_caches_entry() {
        let (sender, _captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        let pp1 = handler.per_pane(42);
        let pp2 = handler.per_pane(42);
        // Same Arc returned for same pane_id
        assert!(Arc::ptr_eq(&pp1, &pp2));

        let pp3 = handler.per_pane(99);
        // Different pane_id gets a different entry
        assert!(!Arc::ptr_eq(&pp1, &pp3));
    }

    #[test]
    fn per_pane_default_has_zero_seqno_and_empty_title() {
        let (sender, _captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);
        let pp = handler.per_pane(1);
        let guard = pp.lock().unwrap();
        assert_eq!(guard.seqno, 0);
        assert_eq!(guard.title, "");
        assert!(!guard.mouse_grabbed);
        assert!(!guard.sent_initial_palette);
        assert!(guard.notifications.is_empty());
        assert!(guard.working_dir.is_none());
    }

    #[test]
    fn tracked_pane_push_does_not_recreate_removed_pane_state() {
        let (sender, _captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        handler.per_pane(7);
        handler.remove_per_pane(7);
        handler.schedule_tracked_pane_push(7);

        assert!(
            handler.per_pane_if_present(7).is_none(),
            "late mux notifications must not recreate cached state for removed panes"
        );
    }

    #[test]
    fn session_owner_retirement_fails_closed_and_releases_its_mux() {
        let mux = Arc::new(Mux::new(None));
        let weak_mux = Arc::downgrade(&mux);
        let owner = SessionOwner::new(mux);
        let authority = owner.authority();
        let operation = authority
            .incarnation
            .try_acquire()
            .expect("live session should admit an operation");

        owner.retire();

        assert!(
            authority.acquire().is_err(),
            "retirement must reject new mux authority"
        );
        assert!(
            authority.try_run(|| ()).is_err(),
            "retirement must reject generic deferred work"
        );
        assert_eq!(
            authority
                .incarnation
                .operation_state
                .load(Ordering::Acquire),
            SESSION_RETIRED | 1,
            "retirement must preserve the admitted operation count until its lease drops"
        );

        drop(operation);
        assert_eq!(
            authority
                .incarnation
                .operation_state
                .load(Ordering::Acquire),
            SESSION_RETIRED,
            "operation release must preserve the retirement fence"
        );
        drop(owner);
        assert!(
            weak_mux.upgrade().is_none(),
            "cloneable deferred authority must not keep the owning mux alive"
        );
    }

    #[test]
    fn per_pane_cache_is_scoped_to_exact_registration() {
        let mux = Arc::new(Mux::new(None));
        let pane_id = 7_001;
        let original: Arc<dyn Pane> = Arc::new(FakePane::new_with_id(pane_id, None));
        mux.add_pane(&original).expect("register original pane");
        let (sender, _captured) = capturing_sender();
        let mut handler =
            SessionHandler::new_for_session(sender, SessionOwner::new(Arc::clone(&mux)));
        let original_registration = handler
            .owner
            .authority()
            .capture_current_pane(pane_id)
            .expect("capture original registration");
        let original_state = handler.per_pane_for_registration(&original_registration);

        assert!(original_registration.retire_if_current());
        let replacement: Arc<dyn Pane> = Arc::new(FakePane::new_with_id(pane_id, None));
        mux.add_pane(&replacement)
            .expect("register replacement pane");
        let replacement_registration = handler
            .owner
            .authority()
            .capture_current_pane(pane_id)
            .expect("capture replacement registration");
        let replacement_state = handler.per_pane_for_registration(&replacement_registration);

        assert!(
            !Arc::ptr_eq(&original_state, &replacement_state),
            "same numeric PaneId must not reuse state across registration generations"
        );
        handler.remove_per_pane_if_same(&original_registration);
        handler.remove_per_pane(pane_id);
        let retained_state = handler
            .per_pane_if_present(pane_id)
            .expect("stale removal signals must preserve a live replacement cache");
        assert!(
            Arc::ptr_eq(&replacement_state, &retained_state),
            "stale exact and raw removals must not clear replacement state"
        );
    }

    #[test]
    fn deferred_pane_read_stays_on_origin_mux_after_global_swap() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let executor = SimpleExecutor::new();
        let pane_id = 7_002;
        let originating_mux = Arc::new(Mux::new(None));
        let replacement_mux = Arc::new(Mux::new(None));
        let originating_pane = Arc::new(FakePane::new_with_id(pane_id, None));
        let replacement_pane = Arc::new(FakePane::new_with_id(pane_id, None));
        originating_pane.state.lock().unwrap().dimensions.cols = 111;
        replacement_pane.state.lock().unwrap().dimensions.cols = 222;
        let originating_pane_dyn: Arc<dyn Pane> = originating_pane;
        let replacement_pane_dyn: Arc<dyn Pane> = replacement_pane;
        originating_mux
            .add_pane(&originating_pane_dyn)
            .expect("register originating pane");
        replacement_mux
            .add_pane(&replacement_pane_dyn)
            .expect("register replacement-mux pane");
        let _mux_guard = ScopedMux::install(&originating_mux);
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new_for_session(
            sender,
            SessionOwner::new(Arc::clone(&originating_mux)),
        );

        handler.process_one(DecodedPdu {
            serial: 301,
            pdu: Pdu::GetPaneRenderableDimensions(GetPaneRenderableDimensions { pane_id }),
        });
        Mux::set_mux(&replacement_mux);
        tick_until_response(&executor, &captured, 1);

        match take_response(&captured).pdu {
            Pdu::GetPaneRenderableDimensionsResponse(GetPaneRenderableDimensionsResponse {
                dimensions,
                ..
            }) => {
                assert_eq!(
                    dimensions.cols, 111,
                    "queued read must resolve against its originating mux"
                );
            }
            other => panic!("expected GetPaneRenderableDimensionsResponse, got {other:?}"),
        }
    }

    #[test]
    fn deferred_kill_rejects_same_id_replacement() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let executor = SimpleExecutor::new();
        let pane_id = 7_003;
        let mux = Arc::new(Mux::new(None));
        let _mux_guard = ScopedMux::install(&mux);
        let original: Arc<dyn Pane> = Arc::new(FakePane::new_with_id(pane_id, None));
        mux.add_pane(&original).expect("register original pane");
        let original_registration = mux
            .capture_current_pane(pane_id)
            .expect("capture original registration");
        let (sender, captured) = capturing_sender();
        let mut handler =
            SessionHandler::new_for_session(sender, SessionOwner::new(Arc::clone(&mux)));

        handler.process_one(DecodedPdu {
            serial: 302,
            pdu: Pdu::KillPane(KillPane { pane_id }),
        });
        assert!(original_registration.retire_if_current());
        let replacement: Arc<dyn Pane> = Arc::new(FakePane::new_with_id(pane_id, None));
        mux.add_pane(&replacement)
            .expect("register replacement pane");
        tick_until_response(&executor, &captured, 1);

        match take_response(&captured).pdu {
            Pdu::ErrorResponse(ErrorResponse { reason }) => {
                assert!(
                    reason.contains("no longer current"),
                    "stale kill should report exact-registration loss, got {reason}"
                );
            }
            other => panic!("expected ErrorResponse, got {other:?}"),
        }
        assert!(
            mux.get_pane(pane_id)
                .is_some_and(|pane| Arc::ptr_eq(&pane, &replacement)),
            "stale queued kill must preserve the same-ID replacement"
        );
    }

    #[test]
    fn dropping_handler_retires_queued_pane_work() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let executor = SimpleExecutor::new();
        let pane_id = 7_004;
        let mux = Arc::new(Mux::new(None));
        let _mux_guard = ScopedMux::install(&mux);
        let pane: Arc<dyn Pane> = Arc::new(FakePane::new_with_id(pane_id, None));
        mux.add_pane(&pane).expect("register pane");
        let registration = mux
            .capture_current_pane(pane_id)
            .expect("capture live pane registration");
        let (sender, captured) = capturing_sender();
        let mut handler =
            SessionHandler::new_for_session(sender, SessionOwner::new(Arc::clone(&mux)));

        handler.process_one(DecodedPdu {
            serial: 303,
            pdu: Pdu::KillPane(KillPane { pane_id }),
        });
        drop(handler);
        let drained = Arc::new(AtomicUsize::new(0));
        let drained_task = Arc::clone(&drained);
        spawn_into_main_thread(async move {
            drained_task.store(1, Ordering::Release);
            Ok::<(), anyhow::Error>(())
        })
        .detach();
        for _ in 0..16 {
            if drained.load(Ordering::Acquire) == 1 {
                break;
            }
            executor.tick().expect("drain queued session work");
        }

        assert_eq!(
            drained.load(Ordering::Acquire),
            1,
            "sentinel must run after queued session work"
        );
        assert!(
            captured.lock().unwrap().is_empty(),
            "retired session must not emit a response from queued work"
        );
        assert_eq!(
            registration.try_with_current(|current| current.pane_id()),
            Some(pane_id),
            "retired session work must not remove its formerly authorized pane"
        );
    }

    #[test]
    fn stale_equal_client_registration_cannot_focus_replacement() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let executor = SimpleExecutor::new();
        let size = test_tab_size();
        let tab = Arc::new(mux::tab::Tab::new(&size));
        let first_pane: Arc<dyn Pane> = Arc::new(FakePane::new_with_id(7_005, None));
        let second_pane: Arc<dyn Pane> = Arc::new(FakePane::new_with_id(7_006, None));
        tab.assign_pane(&first_pane);
        tab.split_and_insert(
            0,
            mux::tab::SplitRequest {
                direction: mux::tab::SplitDirection::Horizontal,
                ..Default::default()
            },
            Arc::clone(&second_pane),
        )
        .expect("insert second pane");
        tab.set_active_pane(&first_pane);
        let (mux, _mux_guard) = install_tab_with_window(&tab, &[Arc::clone(&second_pane)]);
        let client = test_client_id("stale-focus", 41_008);
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);
        handler.process_one(DecodedPdu {
            serial: 304,
            pdu: Pdu::SetClientId(SetClientId {
                client_id: client.clone(),
                is_proxy: false,
            }),
        });
        assert_eq!(
            take_response(&captured).pdu,
            Pdu::UnitResponse(UnitResponse {})
        );
        let stale_client = Arc::clone(handler.client_id.as_ref().expect("registered client"));
        let replacement_client = Arc::new(client);
        mux.register_client(Arc::clone(&replacement_client));
        assert!(!mux.client_registration_is_current(&stale_client));

        handler.process_one(DecodedPdu {
            serial: 305,
            pdu: Pdu::SetFocusedPane(SetFocusedPane {
                pane_id: second_pane.pane_id(),
            }),
        });
        tick_until_response(&executor, &captured, 1);

        match take_response(&captured).pdu {
            Pdu::ErrorResponse(ErrorResponse { reason }) => {
                assert!(
                    reason.contains("client registration is no longer current"),
                    "stale focus should report exact client-registration loss, got {reason}"
                );
            }
            other => panic!("expected ErrorResponse, got {other:?}"),
        }
        assert_eq!(
            tab.get_active_pane().map(|pane| pane.pane_id()),
            Some(first_pane.pane_id()),
            "stale equal-valued client must not focus a replacement registration's pane"
        );
        drop(handler);
        assert!(
            mux.client_registration_is_current(&replacement_client),
            "stale handler cleanup must preserve the equal-valued replacement client"
        );
        assert!(mux.unregister_client_if_same(&replacement_client));
    }

    #[test]
    fn ping_pdu_returns_pong() {
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        handler.process_one(DecodedPdu {
            serial: 42,
            pdu: Pdu::Ping(Ping {}),
        });

        let resp = take_response(&captured);
        assert_eq!(resp.serial, 42);
        assert_eq!(resp.pdu, Pdu::Pong(Pong {}));
    }

    #[test]
    fn select_stack_pane_pdu_selects_requested_stack_member() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let executor = SimpleExecutor::new();
        let size = test_tab_size();
        let tab = Arc::new(mux::tab::Tab::new(&size));
        let pane1: Arc<dyn Pane> = Arc::new(FakePane::new_with_id(1, None));
        let pane2: Arc<dyn Pane> = Arc::new(FakePane::new_with_id(2, None));
        let pane3: Arc<dyn Pane> = Arc::new(FakePane::new_with_id(3, None));
        tab.assign_pane(&pane1);
        let first_request = mux::tab::SplitRequest {
            direction: mux::tab::SplitDirection::Horizontal,
            ..Default::default()
        };
        tab.split_and_insert(0, first_request, Arc::clone(&pane2))
            .unwrap();
        let second_request = mux::tab::SplitRequest {
            direction: mux::tab::SplitDirection::Horizontal,
            ..Default::default()
        };
        tab.split_and_insert(0, second_request, Arc::clone(&pane3))
            .unwrap();
        tab.set_layout_cycle(mux::layout::default_cycle());
        assert_eq!(tab.swap_to_layout_index(2).as_deref(), Some("stacked"));
        let slot_index = tab
            .first_nontrivial_stack_slot_index()
            .expect("three panes in the stacked layout must form a non-trivial stack");
        let stacked_pane_ids = tab.all_stacked_pane_ids();
        assert_eq!(
            stacked_pane_ids.len(),
            3,
            "layout redistribution must preserve every pane in the stack"
        );
        let pane_index = stacked_pane_ids
            .iter()
            .position(|pane_id| *pane_id == pane2.pane_id())
            .expect("the requested pane must remain addressable in the stack");
        let (_mux, _guard) = install_tab_with_window(
            &tab,
            &[Arc::clone(&pane1), Arc::clone(&pane2), Arc::clone(&pane3)],
        );
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        handler.process_one(DecodedPdu {
            serial: 100,
            pdu: Pdu::SelectStackPane(SelectStackPane {
                tab_id: tab.tab_id(),
                slot_index,
                pane_index,
            }),
        });
        tick_until_response(&executor, &captured, 1);

        let resp = take_response(&captured);
        assert_eq!(resp.serial, 100);
        assert_eq!(resp.pdu, Pdu::UnitResponse(UnitResponse {}));
        let visible = tab.iter_panes_ignoring_zoom();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].pane.pane_id(), pane2.pane_id());
        assert_eq!(
            tab.all_stacked_pane_ids().len(),
            3,
            "queued dead-window pruning must retain every registered stack member"
        );
    }

    #[test]
    fn update_pane_constraints_pdu_updates_effective_constraints() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let executor = SimpleExecutor::new();
        let size = test_tab_size();
        let tab = Arc::new(mux::tab::Tab::new(&size));
        let pane: Arc<dyn Pane> = Arc::new(FakePane::new_with_id(7, None));
        tab.assign_pane(&pane);
        let (_mux, _guard) = install_tab_with_window(&tab, &[]);
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        handler.process_one(DecodedPdu {
            serial: 101,
            pdu: Pdu::UpdatePaneConstraints(UpdatePaneConstraints {
                pane_id: 7,
                min_width: Some(20),
                max_width: Some(200),
                min_height: None,
                max_height: Some(50),
            }),
        });
        tick_until_response(&executor, &captured, 1);

        let resp = take_response(&captured);
        assert_eq!(resp.serial, 101);
        assert_eq!(resp.pdu, Pdu::UnitResponse(UnitResponse {}));
        let constraints = tab.effective_pane_constraints(7).unwrap();
        assert_eq!(constraints.min_width, 20);
        assert_eq!(constraints.max_width, Some(200));
        assert_eq!(constraints.min_height, 3);
        assert_eq!(constraints.max_height, Some(50));
    }

    #[test]
    fn floating_pane_pdus_update_live_tab_state() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let executor = SimpleExecutor::new();
        let size = test_tab_size();
        let tab = Arc::new(mux::tab::Tab::new(&size));
        let tiled: Arc<dyn Pane> = Arc::new(FakePane::new_with_id(1, None));
        tab.assign_pane(&tiled);
        let (mux, _guard) = install_tab_with_window(&tab, &[]);
        let floating: Arc<dyn Pane> = Arc::new(FakePane::new_with_id(2, None));
        mux.add_pane(&floating).unwrap();
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        let rect = mux::tab::FloatingPaneRect {
            left: 4,
            top: 5,
            width: 30,
            height: 12,
        };
        handler.process_one(DecodedPdu {
            serial: 110,
            pdu: Pdu::CreateFloatingPane(CreateFloatingPane {
                tab_id: tab.tab_id(),
                pane_id: 2,
                rect,
            }),
        });
        tick_until_response(&executor, &captured, 1);
        assert_eq!(
            take_response(&captured).pdu,
            Pdu::UnitResponse(UnitResponse {})
        );
        assert!(tab.has_floating_pane(2));
        let floating_panes = tab.iter_floating_panes();
        assert_eq!(floating_panes.len(), 1);
        assert_eq!(floating_panes[0].left, 4);

        let moved = mux::tab::FloatingPaneRect {
            left: 10,
            top: 7,
            width: 24,
            height: 10,
        };
        handler.process_one(DecodedPdu {
            serial: 111,
            pdu: Pdu::MoveFloatingPane(MoveFloatingPane {
                pane_id: 2,
                rect: moved,
            }),
        });
        tick_until_response(&executor, &captured, 1);
        assert_eq!(
            take_response(&captured).pdu,
            Pdu::UnitResponse(UnitResponse {})
        );
        let floating_panes = tab.iter_floating_panes();
        assert_eq!(floating_panes[0].left, 10);
        assert_eq!(floating_panes[0].top, 7);

        handler.process_one(DecodedPdu {
            serial: 112,
            pdu: Pdu::SetFloatingPaneZ(SetFloatingPaneZ {
                pane_id: 2,
                z_order: 99,
            }),
        });
        tick_until_response(&executor, &captured, 1);
        assert_eq!(
            take_response(&captured).pdu,
            Pdu::UnitResponse(UnitResponse {})
        );
        assert_eq!(tab.iter_floating_panes()[0].z_order, 99);

        handler.process_one(DecodedPdu {
            serial: 113,
            pdu: Pdu::ToggleFloatingPane(ToggleFloatingPane {
                pane_id: 2,
                visible: false,
            }),
        });
        tick_until_response(&executor, &captured, 1);
        assert_eq!(
            take_response(&captured).pdu,
            Pdu::UnitResponse(UnitResponse {})
        );
        assert!(!tab.iter_floating_panes()[0].visible);

        handler.process_one(DecodedPdu {
            serial: 114,
            pdu: Pdu::RemoveFloatingPane(RemoveFloatingPane { pane_id: 2 }),
        });
        tick_until_response(&executor, &captured, 1);
        assert_eq!(
            take_response(&captured).pdu,
            Pdu::UnitResponse(UnitResponse {})
        );
        assert!(!tab.has_floating_pane(2));
    }

    #[test]
    fn list_panes_releases_window_guard_before_observing_panes() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let executor = SimpleExecutor::new();
        let size = test_tab_size();

        let mux = Arc::new(Mux::new(None));
        let _mux_guard = ScopedMux::install(&mux);
        let window = mux.new_empty_window(None, None);
        let window_id = *window;

        let armed = Arc::new(AtomicUsize::new(0));
        let observations = Arc::new(AtomicUsize::new(0));
        let weak_mux = Arc::downgrade(&mux);
        let armed_for_probe = Arc::clone(&armed);
        let observations_for_probe = Arc::clone(&observations);
        let probe: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            if armed_for_probe.load(Ordering::Acquire) == 0 {
                return;
            }

            let mux = weak_mux.upgrade().expect("test mux remains live");
            assert!(
                mux.try_get_window_mut(window_id).is_some(),
                "ListPanes must release the enclosing mux window read guard \
                 before invoking Pane callbacks",
            );
            observations_for_probe.fetch_add(1, Ordering::AcqRel);
        });

        let pane: Arc<dyn Pane> = Arc::new(FakePane::new_with_callback_probe(7_007, probe));
        let tab = Arc::new(mux::tab::Tab::new(&size));
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab)
            .expect("register test tab and pane");
        mux.add_tab_to_window(&tab, window_id)
            .expect("attach test tab to window");
        drop(window);

        armed.store(1, Ordering::Release);

        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new_for_mux(sender, Arc::clone(&mux));
        handler.process_one(DecodedPdu {
            serial: 121,
            pdu: Pdu::ListPanes(ListPanes {}),
        });
        tick_until_response(&executor, &captured, 1);

        let response = take_response(&captured);
        assert_eq!(response.serial, 121);
        let pdu = response.pdu;
        let Pdu::ListPanesResponse(ListPanesResponse { tabs, .. }) = pdu else {
            panic!("expected ListPanesResponse, got {pdu:?}");
        };
        assert_eq!(tabs.len(), 1);
        assert!(
            observations.load(Ordering::Acquire) > 0,
            "the request must exercise the guarded pane-observation callback",
        );
    }

    #[test]
    fn list_panes_tab_stacks_reports_window_stack_membership() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let executor = SimpleExecutor::new();
        let size = test_tab_size();
        let first = Arc::new(mux::tab::Tab::new(&size));
        let second = Arc::new(mux::tab::Tab::new(&size));
        let first_pane: Arc<dyn Pane> = Arc::new(FakePane::new_with_id(1, None));
        let second_pane: Arc<dyn Pane> = Arc::new(FakePane::new_with_id(2, None));
        first.assign_pane(&first_pane);
        second.assign_pane(&second_pane);
        let first_id = first.tab_id();
        let second_id = second.tab_id();
        let (mux, _guard) = install_tab_with_window(&first, &[]);
        let window_id = mux.iter_windows()[0];
        mux.add_tab_and_active_pane(&second).unwrap();
        mux.add_tab_to_window(&second, window_id).unwrap();
        mux.get_window_mut(window_id)
            .unwrap()
            .create_tab_stack(mux::tab::TabStackId(7), vec![first_id, second_id])
            .unwrap();

        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);
        handler.process_one(DecodedPdu {
            serial: 120,
            pdu: Pdu::ListPanesTabStacks(ListPanesTabStacks {}),
        });
        tick_until_response(&executor, &captured, 1);

        let resp = take_response(&captured);
        assert_eq!(resp.serial, 120);
        match resp.pdu {
            Pdu::ListPanesTabStacksResponse(ListPanesTabStacksResponse { tab_stack_entries }) => {
                assert_eq!(
                    tab_stack_entries,
                    vec![
                        ListPanesTabStackEntry {
                            window_id,
                            stack_id: mux::tab::TabStackId(7),
                            tab_id: first_id,
                            position: 0,
                            is_visible: true,
                        },
                        ListPanesTabStackEntry {
                            window_id,
                            stack_id: mux::tab::TabStackId(7),
                            tab_id: second_id,
                            position: 1,
                            is_visible: false,
                        },
                    ]
                );
            }
            other => panic!("expected ListPanesTabStacksResponse, got {other:?}"),
        }
    }

    #[test]
    fn invalid_pdu_returns_error_response() {
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        handler.process_one(DecodedPdu {
            serial: 200,
            pdu: Pdu::Invalid { ident: 255 },
        });

        let resp = take_response(&captured);
        assert_eq!(resp.serial, 200);
        match resp.pdu {
            Pdu::ErrorResponse(ErrorResponse { reason }) => {
                assert!(
                    reason.contains("invalid PDU"),
                    "error should mention invalid PDU, got: {reason}"
                );
            }
            other => panic!("expected ErrorResponse, got {other:?}"),
        }
    }

    #[test]
    fn response_pdu_treated_as_unexpected_request() {
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        // Sending a Pong (which is a response) as a request should get rejected
        handler.process_one(DecodedPdu {
            serial: 300,
            pdu: Pdu::Pong(Pong {}),
        });

        let resp = take_response(&captured);
        assert_eq!(resp.serial, 300);
        match resp.pdu {
            Pdu::ErrorResponse(ErrorResponse { reason }) => {
                assert!(
                    reason.contains("expected a request"),
                    "error should mention expected request, got: {reason}"
                );
            }
            other => panic!("expected ErrorResponse, got {other:?}"),
        }
    }

    #[test]
    fn unit_response_treated_as_unexpected_request() {
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        handler.process_one(DecodedPdu {
            serial: 301,
            pdu: Pdu::UnitResponse(UnitResponse {}),
        });

        let resp = take_response(&captured);
        match resp.pdu {
            Pdu::ErrorResponse(ErrorResponse { reason }) => {
                assert!(reason.contains("expected a request"));
            }
            other => panic!("expected ErrorResponse, got {other:?}"),
        }
    }

    #[test]
    fn pdu_sender_propagates_errors() {
        let sender = PduSender::new(|_| anyhow::bail!("send failed"));
        let result = sender.send(DecodedPdu {
            serial: 0,
            pdu: Pdu::Pong(Pong {}),
        });
        assert!(result.is_err());
    }

    #[test]
    fn serial_preserved_across_different_pdus() {
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        // Send multiple synchronous PDUs with distinct serials
        for serial in [1, 100, 999, u64::MAX] {
            handler.process_one(DecodedPdu {
                serial,
                pdu: Pdu::Ping(Ping {}),
            });
        }

        let pdus = captured.lock().unwrap();
        let serials: Vec<u64> = pdus.iter().map(|p| p.serial).collect();
        assert_eq!(serials, vec![1, 100, 999, u64::MAX]);
    }

    #[test]
    fn update_pane_constraints_all_none_keeps_default_constraints() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let executor = SimpleExecutor::new();
        let size = test_tab_size();
        let tab = Arc::new(mux::tab::Tab::new(&size));
        let pane: Arc<dyn Pane> = Arc::new(FakePane::new_with_id(1, None));
        tab.assign_pane(&pane);
        let (_mux, _guard) = install_tab_with_window(&tab, &[]);
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        handler.process_one(DecodedPdu {
            serial: 50,
            pdu: Pdu::UpdatePaneConstraints(UpdatePaneConstraints {
                pane_id: 1,
                min_width: None,
                max_width: None,
                min_height: None,
                max_height: None,
            }),
        });
        tick_until_response(&executor, &captured, 1);

        let resp = take_response(&captured);
        assert_eq!(resp.pdu, Pdu::UnitResponse(UnitResponse {}));
        assert_eq!(
            tab.effective_pane_constraints(1).unwrap(),
            mux::pane::PaneConstraints::default()
        );
    }

    #[test]
    fn error_response_pdu_treated_as_unexpected() {
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        handler.process_one(DecodedPdu {
            serial: 400,
            pdu: Pdu::ErrorResponse(ErrorResponse {
                reason: "test error".to_string(),
            }),
        });

        let resp = take_response(&captured);
        match resp.pdu {
            Pdu::ErrorResponse(ErrorResponse { reason }) => {
                assert!(reason.contains("expected a request"));
            }
            other => panic!("expected ErrorResponse, got {other:?}"),
        }
    }

    #[test]
    fn list_panes_response_treated_as_unexpected() {
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        handler.process_one(DecodedPdu {
            serial: 401,
            pdu: Pdu::ListPanesResponse(ListPanesResponse {
                tabs: vec![],
                tab_titles: vec![],
                window_titles: HashMap::new(),
            }),
        });

        let resp = take_response(&captured);
        match resp.pdu {
            Pdu::ErrorResponse(ErrorResponse { reason }) => {
                assert!(reason.contains("expected a request"));
            }
            other => panic!("expected ErrorResponse, got {other:?}"),
        }
    }

    #[test]
    fn list_panes_tab_stacks_response_treated_as_unexpected() {
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        handler.process_one(DecodedPdu {
            serial: 402,
            pdu: Pdu::ListPanesTabStacksResponse(ListPanesTabStacksResponse {
                tab_stack_entries: vec![],
            }),
        });

        let resp = take_response(&captured);
        match resp.pdu {
            Pdu::ErrorResponse(ErrorResponse { reason }) => {
                assert!(reason.contains("expected a request"));
            }
            other => panic!("expected ErrorResponse, got {other:?}"),
        }
    }

    fn test_client_id(name: &str, pid: u32) -> ClientId {
        ClientId {
            hostname: format!("{name}.local"),
            username: "testuser".to_string(),
            pid,
            epoch: 1000,
            id: 0,
            ssh_auth_sock: None,
        }
    }

    #[derive(Clone, Debug)]
    enum LifecycleOp {
        Claim { slot: usize, client_variant: usize },
        Release { slot: usize },
        Ping { slot: usize },
    }

    #[derive(Clone, Debug)]
    enum ReconnectOp {
        Connect { slot: usize, client_variant: usize },
        Release { slot: usize },
        Ping { slot: usize },
    }

    #[derive(Clone, Debug)]
    struct ReconnectRaceStep {
        client_variant: usize,
        drop_stale_before_reconnect: bool,
        drop_stale_after_reconnect: usize,
    }

    #[derive(Clone, Debug)]
    enum ProxyIdentityOp {
        Proxy { proxy_variant: usize },
        RealClient { client_variant: usize },
    }

    #[derive(Clone, Debug)]
    enum ActiveWorkspaceOp {
        SetClient { client_variant: usize },
        SetWorkspace { workspace_variant: usize },
    }

    #[derive(Clone, Debug)]
    enum MissingMuxPaneOp {
        WriteToPane { pane_id: PaneId, data: Vec<u8> },
        SendPaste { pane_id: PaneId, data: String },
        SendKeyDown { pane_id: PaneId },
        SendMouseEvent { pane_id: PaneId },
        GetPaneRenderChanges { pane_id: PaneId },
        KillPane { pane_id: PaneId },
    }

    #[derive(Clone, Debug)]
    enum MissingMuxSessionErrorOp {
        SetWindowWorkspace {
            window_id: usize,
            workspace: String,
        },
        RenameWorkspace {
            old_workspace: String,
            new_workspace: String,
        },
        SetFocusedPane {
            pane_id: PaneId,
        },
        GetClientList,
        ListPanes,
        ListPanesTabStacks,
        WindowTitleChanged {
            window_id: usize,
            title: String,
        },
        TabTitleChanged {
            tab_id: usize,
            title: String,
        },
        SetPaneZoomed {
            tab_id: usize,
            pane_id: PaneId,
            zoomed: bool,
        },
        GetPaneDirection {
            pane_id: PaneId,
            direction: PaneDirection,
        },
        ActivatePaneDirection {
            pane_id: PaneId,
            direction: PaneDirection,
        },
        GetPaneRenderableDimensions {
            pane_id: PaneId,
        },
        GetLines {
            pane_id: PaneId,
            start: StableRowIndex,
            len: StableRowIndex,
        },
        GetImageCell {
            pane_id: PaneId,
            line_idx: StableRowIndex,
            cell_idx: usize,
            data_hash: [u8; 32],
        },
    }

    impl MissingMuxPaneOp {
        fn into_pdu(self) -> Pdu {
            match self {
                Self::WriteToPane { pane_id, data } => {
                    Pdu::WriteToPane(WriteToPane { pane_id, data })
                }
                Self::SendPaste { pane_id, data } => Pdu::SendPaste(SendPaste { pane_id, data }),
                Self::SendKeyDown { pane_id } => Pdu::SendKeyDown(SendKeyDown {
                    pane_id,
                    event: termwiz::input::KeyEvent {
                        key: KeyCode::Char('x'),
                        modifiers: KeyModifiers::NONE,
                    },
                    input_serial: InputSerial::empty(),
                }),
                Self::SendMouseEvent { pane_id } => Pdu::SendMouseEvent(SendMouseEvent {
                    pane_id,
                    event: MouseEvent {
                        kind: wezterm_term::input::MouseEventKind::Move,
                        x: 0,
                        y: 0,
                        x_pixel_offset: 0,
                        y_pixel_offset: 0,
                        button: wezterm_term::input::MouseButton::None,
                        modifiers: KeyModifiers::NONE,
                    },
                }),
                Self::GetPaneRenderChanges { pane_id } => {
                    Pdu::GetPaneRenderChanges(GetPaneRenderChanges { pane_id })
                }
                Self::KillPane { pane_id } => Pdu::KillPane(KillPane { pane_id }),
            }
        }
    }

    impl MissingMuxSessionErrorOp {
        fn into_pdu(self) -> Pdu {
            match self {
                Self::SetWindowWorkspace {
                    window_id,
                    workspace,
                } => Pdu::SetWindowWorkspace(SetWindowWorkspace {
                    window_id,
                    workspace,
                }),
                Self::RenameWorkspace {
                    old_workspace,
                    new_workspace,
                } => Pdu::RenameWorkspace(RenameWorkspace {
                    old_workspace,
                    new_workspace,
                }),
                Self::SetFocusedPane { pane_id } => Pdu::SetFocusedPane(SetFocusedPane { pane_id }),
                Self::GetClientList => Pdu::GetClientList(GetClientList),
                Self::ListPanes => Pdu::ListPanes(ListPanes {}),
                Self::ListPanesTabStacks => Pdu::ListPanesTabStacks(ListPanesTabStacks {}),
                Self::WindowTitleChanged { window_id, title } => {
                    Pdu::WindowTitleChanged(WindowTitleChanged { window_id, title })
                }
                Self::TabTitleChanged { tab_id, title } => {
                    Pdu::TabTitleChanged(TabTitleChanged { tab_id, title })
                }
                Self::SetPaneZoomed {
                    tab_id,
                    pane_id,
                    zoomed,
                } => Pdu::SetPaneZoomed(SetPaneZoomed {
                    containing_tab_id: tab_id,
                    pane_id,
                    zoomed,
                }),
                Self::GetPaneDirection { pane_id, direction } => {
                    Pdu::GetPaneDirection(GetPaneDirection { pane_id, direction })
                }
                Self::ActivatePaneDirection { pane_id, direction } => {
                    Pdu::ActivatePaneDirection(ActivatePaneDirection { pane_id, direction })
                }
                Self::GetPaneRenderableDimensions { pane_id } => {
                    Pdu::GetPaneRenderableDimensions(GetPaneRenderableDimensions { pane_id })
                }
                Self::GetLines {
                    pane_id,
                    start,
                    len,
                } => {
                    let range =
                        stable_row_range_from_signed_len(start, len).unwrap_or(start..start);
                    Pdu::GetLines(GetLines {
                        pane_id,
                        lines: std::iter::once(range).collect(),
                    })
                }
                Self::GetImageCell {
                    pane_id,
                    line_idx,
                    cell_idx,
                    data_hash,
                } => Pdu::GetImageCell(GetImageCell {
                    pane_id,
                    line_idx,
                    cell_idx,
                    data_hash,
                }),
            }
        }
    }

    fn arb_lifecycle_op() -> impl Strategy<Value = LifecycleOp> {
        prop_oneof![
            (0usize..4, 0usize..8).prop_map(|(slot, client_variant)| LifecycleOp::Claim {
                slot,
                client_variant,
            }),
            (0usize..4).prop_map(|slot| LifecycleOp::Release { slot }),
            (0usize..4).prop_map(|slot| LifecycleOp::Ping { slot }),
        ]
    }

    fn arb_reconnect_op() -> impl Strategy<Value = ReconnectOp> {
        prop_oneof![
            (0usize..4, 0usize..6).prop_map(|(slot, client_variant)| ReconnectOp::Connect {
                slot,
                client_variant,
            }),
            (0usize..4).prop_map(|slot| ReconnectOp::Release { slot }),
            (0usize..4).prop_map(|slot| ReconnectOp::Ping { slot }),
        ]
    }

    fn arb_reconnect_race_step() -> impl Strategy<Value = ReconnectRaceStep> {
        (0usize..6, any::<bool>(), 0usize..4).prop_map(
            |(client_variant, drop_stale_before_reconnect, drop_stale_after_reconnect)| {
                ReconnectRaceStep {
                    client_variant,
                    drop_stale_before_reconnect,
                    drop_stale_after_reconnect,
                }
            },
        )
    }

    fn arb_proxy_identity_op() -> impl Strategy<Value = ProxyIdentityOp> {
        prop_oneof![
            (0usize..6).prop_map(|proxy_variant| ProxyIdentityOp::Proxy { proxy_variant }),
            (0usize..8).prop_map(|client_variant| ProxyIdentityOp::RealClient { client_variant }),
        ]
    }

    fn arb_active_workspace_op() -> impl Strategy<Value = ActiveWorkspaceOp> {
        prop_oneof![
            (0usize..8).prop_map(|client_variant| ActiveWorkspaceOp::SetClient { client_variant }),
            (0usize..10).prop_map(|workspace_variant| ActiveWorkspaceOp::SetWorkspace {
                workspace_variant,
            }),
        ]
    }

    fn arb_missing_mux_pane_op() -> impl Strategy<Value = MissingMuxPaneOp> {
        prop_oneof![
            (0usize..4096, proptest::collection::vec(any::<u8>(), 0..64))
                .prop_map(|(pane_id, data)| MissingMuxPaneOp::WriteToPane { pane_id, data }),
            (0usize..4096, "[a-zA-Z0-9 _./-]{0,64}")
                .prop_map(|(pane_id, data)| { MissingMuxPaneOp::SendPaste { pane_id, data } }),
            (0usize..4096).prop_map(|pane_id| MissingMuxPaneOp::SendKeyDown { pane_id }),
            (0usize..4096).prop_map(|pane_id| MissingMuxPaneOp::SendMouseEvent { pane_id }),
            (0usize..4096).prop_map(|pane_id| MissingMuxPaneOp::GetPaneRenderChanges { pane_id }),
            (0usize..4096).prop_map(|pane_id| MissingMuxPaneOp::KillPane { pane_id }),
        ]
    }

    fn arb_pane_direction() -> impl Strategy<Value = PaneDirection> {
        prop_oneof![
            Just(PaneDirection::Left),
            Just(PaneDirection::Right),
            Just(PaneDirection::Up),
            Just(PaneDirection::Down),
            Just(PaneDirection::Next),
            Just(PaneDirection::Prev),
        ]
    }

    fn arb_missing_mux_session_error_op() -> impl Strategy<Value = MissingMuxSessionErrorOp> {
        let label = "[a-zA-Z0-9 _./-]{0,48}";
        let line_start = prop_oneof![
            0isize..128,
            Just(StableRowIndex::MAX - 1),
            Just(StableRowIndex::MAX),
        ];
        let line_len = prop_oneof![1isize..8, Just(-1), Just(2)];
        prop_oneof![
            (0usize..128, label).prop_map(|(window_id, workspace)| {
                MissingMuxSessionErrorOp::SetWindowWorkspace {
                    window_id,
                    workspace,
                }
            }),
            (label, label).prop_map(|(old_workspace, new_workspace)| {
                MissingMuxSessionErrorOp::RenameWorkspace {
                    old_workspace,
                    new_workspace,
                }
            }),
            (0usize..4096).prop_map(|pane_id| MissingMuxSessionErrorOp::SetFocusedPane { pane_id }),
            Just(MissingMuxSessionErrorOp::GetClientList),
            Just(MissingMuxSessionErrorOp::ListPanes),
            Just(MissingMuxSessionErrorOp::ListPanesTabStacks),
            (0usize..128, label).prop_map(|(window_id, title)| {
                MissingMuxSessionErrorOp::WindowTitleChanged { window_id, title }
            }),
            (0usize..128, label).prop_map(|(tab_id, title)| {
                MissingMuxSessionErrorOp::TabTitleChanged { tab_id, title }
            }),
            (0usize..128, 0usize..4096, any::<bool>()).prop_map(|(tab_id, pane_id, zoomed)| {
                MissingMuxSessionErrorOp::SetPaneZoomed {
                    tab_id,
                    pane_id,
                    zoomed,
                }
            },),
            (0usize..4096, arb_pane_direction()).prop_map(|(pane_id, direction)| {
                MissingMuxSessionErrorOp::GetPaneDirection { pane_id, direction }
            }),
            (0usize..4096, arb_pane_direction()).prop_map(|(pane_id, direction)| {
                MissingMuxSessionErrorOp::ActivatePaneDirection { pane_id, direction }
            }),
            (0usize..4096).prop_map(|pane_id| {
                MissingMuxSessionErrorOp::GetPaneRenderableDimensions { pane_id }
            }),
            (0usize..4096, line_start.clone(), line_len).prop_map(|(pane_id, start, len)| {
                MissingMuxSessionErrorOp::GetLines {
                    pane_id,
                    start,
                    len,
                }
            }),
            (0usize..4096, line_start, 0usize..128, any::<[u8; 32]>()).prop_map(
                |(pane_id, line_idx, cell_idx, data_hash)| MissingMuxSessionErrorOp::GetImageCell {
                    pane_id,
                    line_idx,
                    cell_idx,
                    data_hash,
                },
            ),
        ]
    }

    fn client_for_slot(slot: usize, client_variant: usize) -> ClientId {
        test_client_id(
            &format!("prop-slot-{slot}-client-{client_variant}"),
            50_000 + (slot as u32 * 100) + client_variant as u32,
        )
    }

    fn reconnect_client(client_variant: usize) -> ClientId {
        test_client_id(
            &format!("reconnect-client-{client_variant}"),
            60_000 + client_variant as u32,
        )
    }

    fn proxy_client(proxy_variant: usize) -> ClientId {
        test_client_id(
            &format!("proxy-client-{proxy_variant}"),
            70_000 + proxy_variant as u32,
        )
    }

    fn proxied_real_client(client_variant: usize, proxy: Option<&ClientId>) -> ClientId {
        let mut client = test_client_id(
            &format!("proxied-real-client-{client_variant}"),
            80_000 + client_variant as u32,
        );
        if let Some(proxy) = proxy {
            client.ssh_auth_sock.clone_from(&proxy.ssh_auth_sock);
            client.hostname = format!("{} (via proxy pid {})", client.hostname, proxy.pid);
        }
        client
    }

    fn workspace_name(workspace_variant: usize) -> String {
        format!("prop-workspace-{workspace_variant}")
    }

    fn mux_client_set(mux: &Mux) -> HashSet<ClientId> {
        mux.iter_clients()
            .into_iter()
            .map(|info| (*info.client_id).clone())
            .collect()
    }

    fn drop_stale_reconnect_owner(
        mux: &Mux,
        stale: &mut Vec<(SessionHandler, usize, ClientId)>,
        latest_owner_by_client: &mut HashMap<ClientId, usize>,
    ) {
        if let Some((handler, owner, client)) = stale.pop() {
            drop(handler);
            if latest_owner_by_client
                .get(&client)
                .is_some_and(|latest_owner| *latest_owner == owner)
            {
                latest_owner_by_client.remove(&client);
            }
        }

        let expected: HashSet<ClientId> = latest_owner_by_client.keys().cloned().collect();
        assert_eq!(
            mux_client_set(mux),
            expected,
            "mux client set diverged after delayed stale reconnect owner drop"
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

        #[test]
        fn prop_session_handler_claim_release_interleavings_keep_mux_clients_consistent(
            ops in proptest::collection::vec(arb_lifecycle_op(), 1..80)
        ) {
            let _lock = crate::GLOBAL_STATE_TEST_LOCK
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            let mux = Arc::new(Mux::new(None));
            let _mux_guard = ScopedMux::install(&mux);

            struct HandlerSlot {
                handler: Option<SessionHandler>,
                captured: Option<Arc<Mutex<Vec<DecodedPdu>>>>,
            }

            let mut slots: Vec<HandlerSlot> = (0..4)
                .map(|_| HandlerSlot {
                    handler: None,
                    captured: None,
                })
                .collect();
            let mut expected_clients: Vec<Option<ClientId>> = vec![None; 4];

            for (idx, op) in ops.iter().enumerate() {
                let serial = idx as u64 + 1;
                match *op {
                    LifecycleOp::Claim {
                        slot,
                        client_variant,
                    } => {
                        if slots[slot].handler.is_none() {
                            let (sender, captured) = capturing_sender();
                            slots[slot].handler = Some(SessionHandler::new(sender));
                            slots[slot].captured = Some(captured);
                        }

                        let client = client_for_slot(slot, client_variant);
                        slots[slot]
                            .handler
                            .as_mut()
                            .expect("handler exists after claim setup")
                            .process_one(DecodedPdu {
                                serial,
                                pdu: Pdu::SetClientId(SetClientId {
                                    client_id: client.clone(),
                                    is_proxy: false,
                                }),
                            });
                        expected_clients[slot] = Some(client);

                        let captured = slots[slot]
                            .captured
                            .as_ref()
                            .expect("claim slot has captured sender")
                            .lock()
                            .expect("captured response lock");
                        let response = captured.last().expect("SetClientId should respond");
                        prop_assert_eq!(response.serial, serial);
                        prop_assert!(
                            matches!(response.pdu, Pdu::UnitResponse(UnitResponse {})),
                            "claim response at step {idx} should be UnitResponse, got {:?}",
                            response.pdu
                        );
                    }
                    LifecycleOp::Release { slot } => {
                        slots[slot].handler.take();
                        slots[slot].captured.take();
                        expected_clients[slot] = None;
                    }
                    LifecycleOp::Ping { slot } => {
                        if let Some(handler) = slots[slot].handler.as_mut() {
                            handler.process_one(DecodedPdu {
                                serial,
                                pdu: Pdu::Ping(Ping {}),
                            });
                            let captured = slots[slot]
                                .captured
                                .as_ref()
                                .expect("live slot has captured sender")
                                .lock()
                                .expect("captured response lock");
                            let response = captured.last().expect("Ping should respond");
                            prop_assert_eq!(response.serial, serial);
                            prop_assert!(
                                matches!(response.pdu, Pdu::Pong(Pong {})),
                                "ping response at step {idx} should be Pong, got {:?}",
                                response.pdu
                            );
                        }
                    }
                }

                let expected: HashSet<ClientId> = expected_clients
                    .iter()
                    .filter_map(Clone::clone)
                    .collect();
                prop_assert_eq!(
                    mux_client_set(&mux),
                    expected,
                    "mux client set diverged after step {}: {:?}",
                    idx,
                    op
                );
            }

            drop(slots);
            prop_assert!(
                mux.iter_clients().is_empty(),
                "dropping all session handlers should unregister every claimed client"
            );
        }

        #[test]
        fn prop_session_reconnect_latest_owner_controls_mux_registration(
            ops in proptest::collection::vec(arb_reconnect_op(), 1..96)
        ) {
            let _lock = crate::GLOBAL_STATE_TEST_LOCK
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            let mux = Arc::new(Mux::new(None));
            let _mux_guard = ScopedMux::install(&mux);

            struct HandlerSlot {
                handler: Option<SessionHandler>,
                captured: Option<Arc<Mutex<Vec<DecodedPdu>>>>,
            }

            let mut slots: Vec<HandlerSlot> = (0..4)
                .map(|_| HandlerSlot {
                    handler: None,
                    captured: None,
                })
                .collect();
            let mut slot_owner: Vec<Option<(usize, ClientId)>> = vec![None; 4];
            let mut latest_owner_by_client: HashMap<ClientId, usize> = HashMap::new();
            let mut next_owner = 1usize;

            for (idx, op) in ops.iter().enumerate() {
                let serial = idx as u64 + 1;
                match *op {
                    ReconnectOp::Connect {
                        slot,
                        client_variant,
                    } => {
                        if slots[slot].handler.is_none() {
                            let (sender, captured) = capturing_sender();
                            slots[slot].handler = Some(SessionHandler::new(sender));
                            slots[slot].captured = Some(captured);
                        }

                        let client = reconnect_client(client_variant);
                        let registration_is_current =
                            slot_owner[slot]
                                .as_ref()
                                .is_some_and(|(prior_owner, prior_client)| {
                                    *prior_client == client
                                        && latest_owner_by_client
                                            .get(prior_client)
                                            .is_some_and(|current_owner| {
                                                current_owner == prior_owner
                                            })
                                });
                        let owner_changes = !registration_is_current;

                        slots[slot]
                            .handler
                            .as_mut()
                            .expect("handler exists after reconnect setup")
                            .process_one(DecodedPdu {
                                serial,
                                pdu: Pdu::SetClientId(SetClientId {
                                    client_id: client.clone(),
                                    is_proxy: false,
                                }),
                            });

                        if owner_changes {
                            if let Some((prior_owner, prior_client)) = slot_owner[slot].take() {
                                if latest_owner_by_client
                                    .get(&prior_client)
                                    .is_some_and(|owner| *owner == prior_owner)
                                {
                                    latest_owner_by_client.remove(&prior_client);
                                }
                            }
                            let owner = next_owner;
                            next_owner += 1;
                            latest_owner_by_client.insert(client.clone(), owner);
                            slot_owner[slot] = Some((owner, client));
                        }

                        let captured = slots[slot]
                            .captured
                            .as_ref()
                            .expect("reconnect slot has captured sender")
                            .lock()
                            .expect("captured response lock");
                        let response = captured.last().expect("SetClientId should respond");
                        prop_assert_eq!(response.serial, serial);
                        prop_assert!(
                            matches!(response.pdu, Pdu::UnitResponse(UnitResponse {})),
                            "reconnect response at step {idx} should be UnitResponse, got {:?}",
                            response.pdu
                        );
                    }
                    ReconnectOp::Release { slot } => {
                        slots[slot].handler.take();
                        slots[slot].captured.take();
                        if let Some((owner, client)) = slot_owner[slot].take() {
                            if latest_owner_by_client
                                .get(&client)
                                .is_some_and(|latest_owner| *latest_owner == owner)
                            {
                                latest_owner_by_client.remove(&client);
                            }
                        }
                    }
                    ReconnectOp::Ping { slot } => {
                        if let Some(handler) = slots[slot].handler.as_mut() {
                            handler.process_one(DecodedPdu {
                                serial,
                                pdu: Pdu::Ping(Ping {}),
                            });
                            let captured = slots[slot]
                                .captured
                                .as_ref()
                                .expect("live slot has captured sender")
                                .lock()
                                .expect("captured response lock");
                            let response = captured.last().expect("Ping should respond");
                            prop_assert_eq!(response.serial, serial);
                            prop_assert!(
                                matches!(response.pdu, Pdu::Pong(Pong {})),
                                "ping response at step {idx} should be Pong, got {:?}",
                                response.pdu
                            );
                        }
                    }
                }

                let expected: HashSet<ClientId> = latest_owner_by_client.keys().cloned().collect();
                prop_assert_eq!(
                    mux_client_set(&mux),
                    expected,
                    "mux client set diverged after reconnect step {}: {:?}",
                    idx,
                    op
                );
            }

            drop(slots);
            prop_assert!(
                mux.iter_clients().is_empty(),
                "dropping all live reconnect handlers should unregister the current owners"
            );
        }

        #[test]
        fn prop_session_reconnect_delayed_disconnects_preserve_newer_owner(
            steps in proptest::collection::vec(arb_reconnect_race_step(), 1..64)
        ) {
            let _lock = crate::GLOBAL_STATE_TEST_LOCK
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            let mux = Arc::new(Mux::new(None));
            let _mux_guard = ScopedMux::install(&mux);

            let mut active: Option<(SessionHandler, usize, ClientId)> = None;
            let mut stale: Vec<(SessionHandler, usize, ClientId)> = Vec::new();
            let mut latest_owner_by_client: HashMap<ClientId, usize> = HashMap::new();

            for (next_owner, (idx, step)) in (1usize..).zip(steps.iter().enumerate()) {
                if step.drop_stale_before_reconnect {
                    drop_stale_reconnect_owner(&mux, &mut stale, &mut latest_owner_by_client);
                }

                if let Some(prior_active) = active.take() {
                    stale.push(prior_active);
                }

                let (sender, captured) = capturing_sender();
                let mut handler = SessionHandler::new(sender);
                let client = reconnect_client(step.client_variant);
                let owner = next_owner;
                handler.process_one(DecodedPdu {
                    serial: idx as u64 + 1,
                    pdu: Pdu::SetClientId(SetClientId {
                        client_id: client.clone(),
                        is_proxy: false,
                    }),
                });

                let captured = captured.lock().expect("captured response lock");
                let response = captured.last().expect("SetClientId should respond");
                prop_assert_eq!(response.serial, idx as u64 + 1);
                prop_assert!(
                    matches!(response.pdu, Pdu::UnitResponse(UnitResponse {})),
                    "reconnect race step {idx} should return UnitResponse, got {:?}",
                    response.pdu
                );
                drop(captured);

                latest_owner_by_client.insert(client.clone(), owner);
                active = Some((handler, owner, client));

                let expected: HashSet<ClientId> = latest_owner_by_client.keys().cloned().collect();
                prop_assert_eq!(
                    mux_client_set(&mux),
                    expected,
                    "mux client set diverged after reconnect race step {}: {:?}",
                    idx,
                    step
                );

                for _ in 0..step.drop_stale_after_reconnect {
                    if stale.is_empty() {
                        break;
                    }
                    drop_stale_reconnect_owner(&mux, &mut stale, &mut latest_owner_by_client);
                }
            }

            while !stale.is_empty() {
                drop_stale_reconnect_owner(&mux, &mut stale, &mut latest_owner_by_client);
            }
            if let Some((handler, owner, client)) = active.take() {
                drop(handler);
                if latest_owner_by_client
                    .get(&client)
                    .is_some_and(|latest_owner| *latest_owner == owner)
                {
                    latest_owner_by_client.remove(&client);
                }
            }

            prop_assert!(
                latest_owner_by_client.is_empty(),
                "all reconnect owners should be retired at shutdown"
            );
            prop_assert!(
                mux.iter_clients().is_empty(),
                "dropping active plus stale reconnect handlers should unregister every client"
            );
        }

        #[test]
        fn prop_session_concurrent_stale_release_preserves_reclaimed_client(
            client_variant in 0usize..6,
            stale_count in 1usize..8,
        ) {
            let _lock = crate::GLOBAL_STATE_TEST_LOCK
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            let mux = Arc::new(Mux::new(None));
            let _mux_guard = ScopedMux::install(&mux);
            let client = reconnect_client(client_variant);

            let mut stale_handlers = Vec::new();
            for idx in 0..stale_count {
                let (sender, captured) = capturing_sender();
                let mut handler = SessionHandler::new(sender);
                handler.process_one(DecodedPdu {
                    serial: idx as u64 + 1,
                    pdu: Pdu::SetClientId(SetClientId {
                        client_id: client.clone(),
                        is_proxy: false,
                    }),
                });
                let response = take_response(&captured);
                prop_assert_eq!(response.serial, idx as u64 + 1);
                prop_assert!(
                    matches!(response.pdu, Pdu::UnitResponse(UnitResponse {})),
                    "stale setup claim should return UnitResponse, got {:?}",
                    response.pdu
                );
                stale_handlers.push(handler);
            }

            let (sender, captured) = capturing_sender();
            let mut current_handler = SessionHandler::new(sender);
            current_handler.process_one(DecodedPdu {
                serial: 10_000,
                pdu: Pdu::SetClientId(SetClientId {
                    client_id: client.clone(),
                    is_proxy: false,
                }),
            });
            let response = take_response(&captured);
            prop_assert_eq!(response.serial, 10_000);
            prop_assert!(
                matches!(response.pdu, Pdu::UnitResponse(UnitResponse {})),
                "current reclaim should return UnitResponse, got {:?}",
                response.pdu
            );

            let expected_current: HashSet<ClientId> = std::iter::once(client.clone()).collect();
            prop_assert_eq!(
                mux_client_set(&mux),
                expected_current,
                "current handler should own the reclaimed client before stale releases"
            );

            let barrier = Arc::new(Barrier::new(stale_handlers.len() + 1));
            let handles: Vec<_> = stale_handlers
                .into_iter()
                .map(|handler| {
                    let barrier = Arc::clone(&barrier);
                    thread::spawn(move || {
                        barrier.wait();
                        drop(handler);
                    })
                })
                .collect();

            barrier.wait();
            for handle in handles {
                handle
                    .join()
                    .expect("stale handler release thread should not panic");
            }

            let expected_after_release: HashSet<ClientId> = std::iter::once(client.clone()).collect();
            prop_assert_eq!(
                mux_client_set(&mux),
                expected_after_release,
                "concurrent stale releases must not clear the reclaimed client"
            );

            drop(current_handler);
            prop_assert!(
                mux.iter_clients().is_empty(),
                "dropping the current owner should unregister the reclaimed client"
            );
        }

        #[test]
        fn prop_session_concurrent_stale_release_preserves_reused_pane_state(
            client_variant in 0usize..6,
            pane_id in 1usize..4096,
            stale_count in 1usize..8,
        ) {
            let _lock = crate::GLOBAL_STATE_TEST_LOCK
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            let executor = SimpleExecutor::new();
            let size = test_tab_size();
            let tab = Arc::new(mux::tab::Tab::new(&size));
            let pane: Arc<dyn Pane> = Arc::new(FakePane::new_with_id(pane_id, None));
            tab.assign_pane(&pane);
            let (mux, _mux_guard) = install_tab_with_window(&tab, &[]);
            let client = reconnect_client(client_variant);

            let mut stale_handlers = Vec::new();
            for idx in 0..stale_count {
                let (sender, captured) = capturing_sender();
                let mut handler = SessionHandler::new(sender);
                handler.process_one(DecodedPdu {
                    serial: idx as u64 + 1,
                    pdu: Pdu::SetClientId(SetClientId {
                        client_id: client.clone(),
                        is_proxy: false,
                    }),
                });
                let response = take_response(&captured);
                prop_assert_eq!(response.serial, idx as u64 + 1);
                prop_assert!(
                    matches!(response.pdu, Pdu::UnitResponse(UnitResponse {})),
                    "stale setup claim should return UnitResponse, got {:?}",
                    response.pdu
                );

                handler.process_one(DecodedPdu {
                    serial: 1_000 + idx as u64,
                    pdu: Pdu::GetPaneRenderChanges(GetPaneRenderChanges { pane_id }),
                });
                tick_until_response(&executor, &captured, 2);
                prop_assert!(
                    handler.per_pane_if_present(pane_id).is_some(),
                    "stale handler should track reused pane id before release"
                );
                stale_handlers.push(handler);
            }

            let (sender, captured) = capturing_sender();
            let mut current_handler = SessionHandler::new(sender);
            current_handler.process_one(DecodedPdu {
                serial: 10_000,
                pdu: Pdu::SetClientId(SetClientId {
                    client_id: client.clone(),
                    is_proxy: false,
                }),
            });
            let response = take_response(&captured);
            prop_assert_eq!(response.serial, 10_000);
            prop_assert!(
                matches!(response.pdu, Pdu::UnitResponse(UnitResponse {})),
                "current reclaim should return UnitResponse, got {:?}",
                response.pdu
            );

            current_handler.process_one(DecodedPdu {
                serial: 10_001,
                pdu: Pdu::GetPaneRenderChanges(GetPaneRenderChanges { pane_id }),
            });
            tick_until_response(&executor, &captured, 2);
            let first_pane_state = current_handler
                .per_pane_if_present(pane_id)
                .expect("current handler should track pane before reuse");

            let first_registration = mux
                .capture_current_pane(pane_id)
                .expect("capture pane registration before reuse");
            prop_assert!(
                first_registration.retire_if_current(),
                "the original pane registration should retire before reuse"
            );
            let replacement: Arc<dyn Pane> =
                Arc::new(FakePane::new_with_id(pane_id, None));
            mux.add_pane(&replacement)
                .expect("same-ID replacement should register");
            current_handler.process_one(DecodedPdu {
                serial: 10_002,
                pdu: Pdu::GetPaneRenderChanges(GetPaneRenderChanges { pane_id }),
            });
            tick_until_response(&executor, &captured, 4);
            let reused_pane_state = current_handler
                .per_pane_if_present(pane_id)
                .expect("current handler should recreate state for reused pane id");
            prop_assert!(
                !Arc::ptr_eq(&first_pane_state, &reused_pane_state),
                "PaneId reuse should allocate fresh per-pane state after removal"
            );
            current_handler.remove_per_pane_if_same(&first_registration);
            current_handler.remove_per_pane(pane_id);
            let retained_reused_state = current_handler
                .per_pane_if_present(pane_id)
                .expect("stale removal signals must preserve live reused pane state");
            prop_assert!(
                Arc::ptr_eq(&reused_pane_state, &retained_reused_state),
                "stale exact and raw removal signals must preserve replacement state"
            );

            let expected_current: HashSet<ClientId> = std::iter::once(client.clone()).collect();
            prop_assert_eq!(
                mux_client_set(&mux),
                expected_current,
                "current handler should own the reclaimed client before stale releases"
            );

            let barrier = Arc::new(Barrier::new(stale_handlers.len() + 1));
            let handles: Vec<_> = stale_handlers
                .into_iter()
                .map(|handler| {
                    let barrier = Arc::clone(&barrier);
                    thread::spawn(move || {
                        barrier.wait();
                        drop(handler);
                    })
                })
                .collect();

            barrier.wait();
            for handle in handles {
                handle
                    .join()
                    .expect("stale handler release thread should not panic");
            }

            let expected_after_release: HashSet<ClientId> = std::iter::once(client.clone()).collect();
            prop_assert_eq!(
                mux_client_set(&mux),
                expected_after_release,
                "concurrent stale releases must not clear the reclaimed client with reused PaneId"
            );
            let after_release_pane_state = current_handler
                .per_pane_if_present(pane_id)
                .expect("stale releases must not clear current reused pane state");
            prop_assert!(
                Arc::ptr_eq(&reused_pane_state, &after_release_pane_state),
                "stale releases must not replace current reused pane state"
            );

            drop(current_handler);
            prop_assert!(
                mux.iter_clients().is_empty(),
                "dropping the current owner should unregister the reclaimed client"
            );
        }

        #[test]
        fn prop_session_proxy_identity_sequences_register_only_real_clients(
            ops in proptest::collection::vec(arb_proxy_identity_op(), 1..80)
        ) {
            let _lock = crate::GLOBAL_STATE_TEST_LOCK
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            let mux = Arc::new(Mux::new(None));
            let _mux_guard = ScopedMux::install(&mux);
            let (sender, captured) = capturing_sender();
            let mut handler = SessionHandler::new(sender);
            let mut first_proxy: Option<ClientId> = None;
            let mut expected_registered: Option<ClientId> = None;

            for (idx, op) in ops.iter().enumerate() {
                let serial = idx as u64 + 1;
                match *op {
                    ProxyIdentityOp::Proxy { proxy_variant } => {
                        let proxy = proxy_client(proxy_variant);
                        handler.process_one(DecodedPdu {
                            serial,
                            pdu: Pdu::SetClientId(SetClientId {
                                client_id: proxy.clone(),
                                is_proxy: true,
                            }),
                        });
                        if first_proxy.is_none() {
                            first_proxy = Some(proxy);
                        }
                    }
                    ProxyIdentityOp::RealClient { client_variant } => {
                        let raw_client = proxied_real_client(client_variant, None);
                        handler.process_one(DecodedPdu {
                            serial,
                            pdu: Pdu::SetClientId(SetClientId {
                                client_id: raw_client,
                                is_proxy: false,
                            }),
                        });
                        expected_registered =
                            Some(proxied_real_client(client_variant, first_proxy.as_ref()));
                    }
                }

                let captured = captured.lock().expect("captured response lock");
                let response = captured.last().expect("SetClientId should respond");
                prop_assert_eq!(response.serial, serial);
                prop_assert!(
                    matches!(response.pdu, Pdu::UnitResponse(UnitResponse {})),
                    "proxy identity op at step {idx} should return UnitResponse, got {:?}",
                    response.pdu
                );
                drop(captured);

                prop_assert_eq!(
                    handler.proxy_client_id.as_ref(),
                    first_proxy.as_ref(),
                    "first proxy identity must remain sticky after step {}: {:?}",
                    idx,
                    op
                );
                let expected: HashSet<ClientId> = expected_registered.iter().cloned().collect();
                prop_assert_eq!(
                    mux_client_set(&mux),
                    expected,
                    "proxy identity sequence registered wrong clients after step {}: {:?}",
                    idx,
                    op
                );
            }

            drop(handler);
            prop_assert!(
                mux.iter_clients().is_empty(),
                "dropping proxy identity handler should unregister the current real client"
            );
        }

        #[test]
        fn prop_session_active_workspace_sequences_follow_current_client(
            ops in proptest::collection::vec(arb_active_workspace_op(), 1..80)
        ) {
            let _lock = crate::GLOBAL_STATE_TEST_LOCK
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            let executor = SimpleExecutor::new();
            let mux = Arc::new(Mux::new(None));
            let _mux_guard = ScopedMux::install(&mux);
            let (sender, captured) = capturing_sender();
            let mut handler = SessionHandler::new(sender);
            let default_workspace = mux.active_workspace();
            let mut expected_client: Option<ClientId> = None;
            let mut expected_workspace: Option<String> = None;

            for (idx, op) in ops.iter().enumerate() {
                let serial = idx as u64 + 1;
                let mut expect_workspace_error = false;
                match *op {
                    ActiveWorkspaceOp::SetClient { client_variant } => {
                        let client = reconnect_client(client_variant);
                        let same_client = expected_client.as_ref().is_some_and(|prior| *prior == client);
                        handler.process_one(DecodedPdu {
                            serial,
                            pdu: Pdu::SetClientId(SetClientId {
                                client_id: client.clone(),
                                is_proxy: false,
                            }),
                        });
                        if !same_client {
                            expected_client = Some(client);
                            expected_workspace = None;
                        }
                    }
                    ActiveWorkspaceOp::SetWorkspace { workspace_variant } => {
                        let workspace = workspace_name(workspace_variant);
                        expect_workspace_error = expected_client.is_none();
                        handler.process_one(DecodedPdu {
                            serial,
                            pdu: Pdu::SetActiveWorkspace(SetActiveWorkspace { workspace: workspace.clone() }),
                        });
                        if !expect_workspace_error {
                            expected_workspace = Some(workspace);
                        }
                    }
                }

                tick_until_response(&executor, &captured, idx + 1);
                let captured_guard = captured.lock().expect("captured response lock");
                let response = captured_guard
                    .last()
                    .expect("workspace sequence op should respond");
                prop_assert_eq!(response.serial, serial);
                if expect_workspace_error {
                    match &response.pdu {
                        Pdu::ErrorResponse(ErrorResponse { reason }) => {
                            prop_assert!(
                                reason.contains("set active workspace before SetClientId"),
                                "pre-client workspace update returned wrong error after step {}: {:?}: {reason}",
                                idx,
                                op
                            );
                        }
                        other => {
                            prop_assert!(
                                false,
                                "pre-client workspace update should return ErrorResponse after step {}: {:?}, got {other:?}",
                                idx,
                                op
                            );
                        }
                    }
                } else {
                    prop_assert!(
                        matches!(response.pdu, Pdu::UnitResponse(UnitResponse {})),
                        "workspace sequence op at step {} should return UnitResponse, got {:?}",
                        idx,
                        response.pdu
                    );
                }
                drop(captured_guard);

                let expected_clients: HashSet<ClientId> = expected_client.iter().cloned().collect();
                prop_assert_eq!(
                    mux_client_set(&mux),
                    expected_clients,
                    "workspace sequence registered wrong clients after step {}: {:?}",
                    idx,
                    op
                );
                if let Some(client) = expected_client.as_ref() {
                    let registered_client = handler
                        .client_id
                        .as_ref()
                        .expect("expected client must have an exact registration");
                    prop_assert_eq!(
                        registered_client.as_ref(),
                        client,
                        "handler retained the wrong exact client after step {}: {:?}",
                        idx,
                        op,
                    );
                    prop_assert!(
                        mux.client_registration_is_current(registered_client),
                        "handler retained a stale client registration after step {}: {:?}",
                        idx,
                        op,
                    );
                    let expected_workspace = expected_workspace
                        .as_deref()
                        .unwrap_or(default_workspace.as_str());
                    prop_assert_eq!(
                        mux.active_workspace_for_client(registered_client),
                        expected_workspace,
                        "active workspace drifted after step {}: {:?}",
                        idx,
                        op
                    );
                } else {
                    prop_assert!(
                        handler.client_id.is_none(),
                        "handler retained a client before SetClientId after step {}: {:?}",
                        idx,
                        op,
                    );
                }
            }

            drop(handler);
            prop_assert!(
                mux.iter_clients().is_empty(),
                "dropping active workspace handler should unregister the current client"
            );
        }

        #[test]
        fn prop_private_session_missing_pane_paths_do_not_retain_per_pane_state(
            ops in proptest::collection::vec(arb_missing_mux_pane_op(), 1..64)
        ) {
            let _lock = crate::GLOBAL_STATE_TEST_LOCK
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            let executor = SimpleExecutor::new();
            let _mux_guard = ScopedMux::shutdown_current();
            let (sender, captured) = capturing_sender();
            let mut handler = SessionHandler::new(sender);

            for (idx, op) in ops.into_iter().enumerate() {
                handler.process_one(DecodedPdu {
                    serial: idx as u64 + 1,
                    pdu: op.clone().into_pdu(),
                });
                tick_until_response(&executor, &captured, idx + 1);

                let captured = captured.lock().expect("captured response lock");
                let response = captured.last().expect("cancel path should respond");
                prop_assert_eq!(response.serial, idx as u64 + 1);
                match (&op, &response.pdu) {
                    (
                        MissingMuxPaneOp::GetPaneRenderChanges { pane_id },
                        Pdu::LivenessResponse(LivenessResponse {
                            pane_id: response_pane_id,
                            is_alive: false,
                        }),
                    ) => {
                        prop_assert_eq!(response_pane_id, pane_id);
                    }
                    (_, Pdu::ErrorResponse(ErrorResponse { reason })) => {
                        prop_assert!(
                            reason.contains("no such pane"),
                            "private-session missing-pane path should report absence for {:?}, got {reason}",
                            op
                        );
                    }
                    other => {
                        prop_assert!(
                            false,
                            "private-session missing-pane path for {:?} returned {other:?}",
                            op
                        );
                    }
                }
                drop(captured);

                prop_assert!(
                    handler.per_pane.is_empty(),
                    "private-session missing-pane path retained per-pane state after step {}: {:?}",
                    idx,
                    op
                );
            }
        }

        #[test]
        fn prop_present_mux_missing_pane_paths_do_not_retain_per_pane_state(
            ops in proptest::collection::vec(arb_missing_mux_pane_op(), 1..64)
        ) {
            let _lock = crate::GLOBAL_STATE_TEST_LOCK
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            let executor = SimpleExecutor::new();
            let mux = Arc::new(Mux::new(None));
            let _mux_guard = ScopedMux::install(&mux);
            let (sender, captured) = capturing_sender();
            let mut handler = SessionHandler::new(sender);

            for (idx, op) in ops.into_iter().enumerate() {
                let serial = idx as u64 + 1;
                handler.process_one(DecodedPdu {
                    serial,
                    pdu: op.clone().into_pdu(),
                });
                tick_until_response(&executor, &captured, idx + 1);

                let captured = captured.lock().expect("captured response lock");
                let response = captured.last().expect("missing-pane path should respond");
                prop_assert_eq!(response.serial, serial);
                match (&op, &response.pdu) {
                    (
                        MissingMuxPaneOp::GetPaneRenderChanges { pane_id },
                        Pdu::LivenessResponse(LivenessResponse {
                            pane_id: response_pane_id,
                            is_alive: false,
                        }),
                    ) => {
                        prop_assert_eq!(response_pane_id, pane_id);
                    }
                    (_, Pdu::ErrorResponse(ErrorResponse { reason })) => {
                        prop_assert!(
                            reason.contains("no such pane"),
                            "present-mux missing-pane path should report missing pane for {:?}, got {reason}",
                            op
                        );
                    }
                    other => {
                        prop_assert!(
                            false,
                            "present-mux missing-pane path for {:?} returned {other:?}",
                            op
                        );
                    }
                }
                drop(captured);

                prop_assert!(
                    handler.per_pane.is_empty(),
                    "present-mux missing-pane path retained per-pane state after step {}: {:?}",
                    idx,
                    op
                );
            }
        }

        #[test]
        fn prop_private_session_empty_topology_paths_preserve_serial_and_state(
            ops in proptest::collection::vec(arb_missing_mux_session_error_op(), 1..64)
        ) {
            let _lock = crate::GLOBAL_STATE_TEST_LOCK
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            let executor = SimpleExecutor::new();
            let _mux_guard = ScopedMux::shutdown_current();
            let (sender, captured) = capturing_sender();
            let mut handler = SessionHandler::new(sender);

            for (idx, op) in ops.into_iter().enumerate() {
                let serial = idx as u64 + 1;
                handler.process_one(DecodedPdu {
                    serial,
                    pdu: op.clone().into_pdu(),
                });
                tick_until_response(&executor, &captured, idx + 1);

                let captured = captured.lock().expect("captured response lock");
                let response = captured
                    .last()
                    .expect("empty private-session path should respond");
                prop_assert_eq!(response.serial, serial);
                match (&op, &response.pdu) {
                    (
                        MissingMuxSessionErrorOp::RenameWorkspace { .. },
                        Pdu::UnitResponse(UnitResponse {}),
                    ) => {}
                    (
                        MissingMuxSessionErrorOp::GetClientList,
                        Pdu::GetClientListResponse(GetClientListResponse { clients }),
                    ) => {
                        prop_assert!(clients.is_empty());
                    }
                    (
                        MissingMuxSessionErrorOp::ListPanes,
                        Pdu::ListPanesResponse(ListPanesResponse {
                            tabs,
                            tab_titles,
                            window_titles,
                        }),
                    ) => {
                        prop_assert!(tabs.is_empty());
                        prop_assert!(tab_titles.is_empty());
                        prop_assert!(window_titles.is_empty());
                    }
                    (
                        MissingMuxSessionErrorOp::ListPanesTabStacks,
                        Pdu::ListPanesTabStacksResponse(ListPanesTabStacksResponse {
                            tab_stack_entries,
                        }),
                    ) => {
                        prop_assert!(tab_stack_entries.is_empty());
                    }
                    (request, Pdu::ErrorResponse(ErrorResponse { reason })) => {
                        let expected = match request {
                            MissingMuxSessionErrorOp::SetWindowWorkspace { .. } => "window",
                            MissingMuxSessionErrorOp::WindowTitleChanged { .. } => {
                                "no such window"
                            }
                            MissingMuxSessionErrorOp::TabTitleChanged { .. } => "no such tab",
                            MissingMuxSessionErrorOp::SetFocusedPane { .. }
                            | MissingMuxSessionErrorOp::SetPaneZoomed { .. }
                            | MissingMuxSessionErrorOp::GetPaneDirection { .. }
                            | MissingMuxSessionErrorOp::ActivatePaneDirection { .. }
                            | MissingMuxSessionErrorOp::GetPaneRenderableDimensions { .. }
                            | MissingMuxSessionErrorOp::GetLines { .. }
                            | MissingMuxSessionErrorOp::GetImageCell { .. } => "no such pane",
                            MissingMuxSessionErrorOp::RenameWorkspace { .. }
                            | MissingMuxSessionErrorOp::GetClientList
                            | MissingMuxSessionErrorOp::ListPanes
                            | MissingMuxSessionErrorOp::ListPanesTabStacks => {
                                prop_assert!(
                                    false,
                                    "successful empty-session request returned ErrorResponse: {:?}: {reason}",
                                    request
                                );
                                ""
                            }
                        };
                        prop_assert!(
                            reason.contains(expected),
                            "empty private-session path returned wrong error for {:?}: {reason}",
                            request
                        );
                    }
                    other => {
                        prop_assert!(
                            false,
                            "empty private-session path for {:?} returned {other:?}",
                            op
                        );
                    }
                }
                drop(captured);

                prop_assert!(
                    handler.client_id.is_none(),
                    "empty private-session path retained client identity after step {}: {:?}",
                    idx,
                    op
                );
                prop_assert!(
                    handler.per_pane.is_empty(),
                    "empty private-session path retained per-pane state after step {}: {:?}",
                    idx,
                    op
                );
            }
        }
    }

    #[test]
    fn set_client_id_proxy_stores_proxy_identity() {
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        let proxy_id = test_client_id("proxy-host", 999);
        handler.process_one(DecodedPdu {
            serial: 500,
            pdu: Pdu::SetClientId(SetClientId {
                client_id: proxy_id.clone(),
                is_proxy: true,
            }),
        });

        let resp = take_response(&captured);
        assert_eq!(resp.serial, 500);
        assert_eq!(resp.pdu, Pdu::UnitResponse(UnitResponse {}));

        // Proxy identity should be stored
        assert!(handler.proxy_client_id.is_some());
        let stored = handler.proxy_client_id.as_ref().unwrap();
        assert_eq!(stored.hostname, "proxy-host.local");
        assert_eq!(stored.pid, 999);
    }

    #[test]
    fn set_client_id_proxy_only_stores_first_proxy() {
        let (sender, _captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        let first_proxy = test_client_id("first-proxy", 100);
        handler.process_one(DecodedPdu {
            serial: 1,
            pdu: Pdu::SetClientId(SetClientId {
                client_id: first_proxy,
                is_proxy: true,
            }),
        });

        let second_proxy = test_client_id("second-proxy", 200);
        handler.process_one(DecodedPdu {
            serial: 2,
            pdu: Pdu::SetClientId(SetClientId {
                client_id: second_proxy,
                is_proxy: true,
            }),
        });

        // First proxy should be kept, second ignored
        let stored = handler.proxy_client_id.as_ref().unwrap();
        assert_eq!(stored.hostname, "first-proxy.local");
        assert_eq!(stored.pid, 100);
    }

    #[test]
    fn set_client_id_replaces_prior_registered_client_without_leaking_stale_entries() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let first = test_client_id("review-first", 41_001);
        let second = test_client_id("review-second", 41_002);
        let mux = Arc::new(Mux::new(None));
        let _mux_guard = ScopedMux::install(&mux);
        let (sender, _captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        handler.process_one(DecodedPdu {
            serial: 10,
            pdu: Pdu::SetClientId(SetClientId {
                client_id: first.clone(),
                is_proxy: false,
            }),
        });

        let clients_after_first_set = mux.iter_clients();
        assert!(
            clients_after_first_set
                .iter()
                .any(|info| *info.client_id == first)
        );

        handler.process_one(DecodedPdu {
            serial: 11,
            pdu: Pdu::SetClientId(SetClientId {
                client_id: second.clone(),
                is_proxy: false,
            }),
        });

        let clients = mux.iter_clients();
        assert!(clients.iter().any(|info| *info.client_id == second));
        assert!(!clients.iter().any(|info| *info.client_id == first));

        drop(handler);

        let clients_after_drop = mux.iter_clients();
        assert!(
            !clients_after_drop
                .iter()
                .any(|info| *info.client_id == second)
        );
    }

    #[test]
    fn same_value_client_reclaims_after_equal_replacement_departed() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let mux = Arc::new(Mux::new(None));
        let _mux_guard = ScopedMux::install(&mux);
        let client_five = reconnect_client(5);
        let client_zero = reconnect_client(0);

        let (slot_two_sender, slot_two_responses) = capturing_sender();
        let mut slot_two = SessionHandler::new(slot_two_sender);
        slot_two.process_one(DecodedPdu {
            serial: 1,
            pdu: Pdu::SetClientId(SetClientId {
                client_id: client_five.clone(),
                is_proxy: false,
            }),
        });
        assert!(matches!(
            take_response(&slot_two_responses).pdu,
            Pdu::UnitResponse(UnitResponse {})
        ));
        let slot_two_first = Arc::clone(
            slot_two
                .client_id
                .as_ref()
                .expect("slot two must retain its first exact registration"),
        );
        assert!(mux.client_registration_is_current(&slot_two_first));

        let (slot_three_sender, slot_three_responses) = capturing_sender();
        let mut slot_three = SessionHandler::new(slot_three_sender);
        slot_three.process_one(DecodedPdu {
            serial: 2,
            pdu: Pdu::SetClientId(SetClientId {
                client_id: client_five.clone(),
                is_proxy: false,
            }),
        });
        assert!(matches!(
            take_response(&slot_three_responses).pdu,
            Pdu::UnitResponse(UnitResponse {})
        ));
        let slot_three_client_five = Arc::clone(
            slot_three
                .client_id
                .as_ref()
                .expect("slot three must retain the equal replacement"),
        );
        assert!(mux.client_registration_is_current(&slot_three_client_five));
        assert!(!mux.client_registration_is_current(&slot_two_first));

        slot_three.process_one(DecodedPdu {
            serial: 3,
            pdu: Pdu::SetClientId(SetClientId {
                client_id: client_zero.clone(),
                is_proxy: false,
            }),
        });
        assert!(matches!(
            take_response(&slot_three_responses).pdu,
            Pdu::UnitResponse(UnitResponse {})
        ));
        assert_eq!(mux_client_set(&mux), HashSet::from([client_zero.clone()]));

        slot_two.process_one(DecodedPdu {
            serial: 4,
            pdu: Pdu::SetClientId(SetClientId {
                client_id: client_five.clone(),
                is_proxy: false,
            }),
        });
        assert!(matches!(
            take_response(&slot_two_responses).pdu,
            Pdu::UnitResponse(UnitResponse {})
        ));

        let slot_two_reclaimed = Arc::clone(
            slot_two
                .client_id
                .as_ref()
                .expect("slot two must retain its reclaimed registration"),
        );
        assert!(
            !Arc::ptr_eq(&slot_two_first, &slot_two_reclaimed),
            "reclaim must retain the newly received exact token",
        );
        assert!(mux.client_registration_is_current(&slot_two_reclaimed));
        assert!(!mux.client_registration_is_current(&slot_two_first));
        assert_eq!(
            mux_client_set(&mux),
            HashSet::from([client_zero.clone(), client_five.clone()]),
        );

        drop(slot_three);
        assert_eq!(mux_client_set(&mux), HashSet::from([client_five]));
        drop(slot_two);
        assert!(mux.iter_clients().is_empty());
    }

    #[test]
    fn handler_drop_unregisters_from_owning_mux_after_global_replacement() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let client = test_client_id("exact-mux-owner", 41_006);
        let originating_mux = Arc::new(Mux::new(None));
        let replacement_mux = Arc::new(Mux::new(None));
        let _mux_guard = ScopedMux::install(&originating_mux);
        let (sender, _captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        handler.process_one(DecodedPdu {
            serial: 14,
            pdu: Pdu::SetClientId(SetClientId {
                client_id: client.clone(),
                is_proxy: false,
            }),
        });
        let replacement_client = Arc::new(client.clone());
        replacement_mux.register_client(Arc::clone(&replacement_client));
        Mux::set_mux(&replacement_mux);

        drop(handler);

        assert!(
            originating_mux.iter_clients().is_empty(),
            "handler cleanup must unregister from the mux that owns its registration",
        );
        assert!(
            replacement_mux.client_registration_is_current(&replacement_client),
            "cleanup from an old mux must not remove an equal-valued replacement registration",
        );
    }

    #[test]
    fn repeated_client_identity_stays_bound_to_connection_mux_after_global_replacement() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let client = test_client_id("mux-rebind", 41_007);
        let originating_mux = Arc::new(Mux::new(None));
        let replacement_mux = Arc::new(Mux::new(None));
        let _mux_guard = ScopedMux::install(&originating_mux);
        let (sender, _captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        for serial in [15, 16] {
            if serial == 16 {
                Mux::set_mux(&replacement_mux);
            }
            handler.process_one(DecodedPdu {
                serial,
                pdu: Pdu::SetClientId(SetClientId {
                    client_id: client.clone(),
                    is_proxy: false,
                }),
            });
        }

        assert!(
            originating_mux
                .iter_clients()
                .iter()
                .any(|info| *info.client_id == client),
            "repeated identity must remain registered on the connection's mux",
        );
        assert!(
            replacement_mux.iter_clients().is_empty(),
            "changing the process-global mux must not retarget a live connection",
        );
        drop(handler);
        assert!(
            originating_mux.iter_clients().is_empty(),
            "drop must clean up the connection-owned registration",
        );
    }

    #[test]
    fn dropping_handler_after_global_mux_shutdown_cleans_its_connection_mux() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let mux = Arc::new(Mux::new(None));
        let _mux_guard = ScopedMux::install(&mux);
        let client = test_client_id("shutdown-safe", 41_003);
        let (sender, _captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        handler.process_one(DecodedPdu {
            serial: 12,
            pdu: Pdu::SetClientId(SetClientId {
                client_id: client,
                is_proxy: false,
            }),
        });

        Mux::shutdown();
        drop(handler);
        assert!(
            mux.iter_clients().is_empty(),
            "global shutdown must not prevent exact connection-owner cleanup"
        );
    }

    #[test]
    fn set_client_id_without_global_mux_uses_handler_owned_mux() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let _mux_guard = ScopedMux::shutdown_current();
        let client = test_client_id("missing-mux", 41_005);
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);
        let owned_mux = Arc::clone(handler.owner.mux());

        handler.process_one(DecodedPdu {
            serial: 13,
            pdu: Pdu::SetClientId(SetClientId {
                client_id: client.clone(),
                is_proxy: false,
            }),
        });

        let resp = take_response(&captured);
        assert_eq!(resp.serial, 13);
        assert_eq!(resp.pdu, Pdu::UnitResponse(UnitResponse {}));
        assert!(
            owned_mux
                .iter_clients()
                .iter()
                .any(|info| *info.client_id == client),
            "a handler created without a global mux must retain its private owner"
        );
        assert!(Mux::try_get().is_none());
        drop(handler);
        assert!(
            owned_mux.iter_clients().is_empty(),
            "dropping the handler must unregister from its private mux"
        );
    }

    #[test]
    fn set_active_workspace_updates_registered_client_workspace() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let executor = SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
        let _mux_guard = ScopedMux::install(&mux);
        let client = test_client_id("workspace-owner", 41_004);
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        handler.process_one(DecodedPdu {
            serial: 21,
            pdu: Pdu::SetClientId(SetClientId {
                client_id: client.clone(),
                is_proxy: false,
            }),
        });
        let _ = take_response(&captured);

        handler.process_one(DecodedPdu {
            serial: 22,
            pdu: Pdu::SetActiveWorkspace(SetActiveWorkspace {
                workspace: "remote-dev".to_string(),
            }),
        });
        tick_until_response(&executor, &captured, 1);

        let resp = take_response(&captured);
        assert_eq!(resp.serial, 22);
        assert_eq!(resp.pdu, Pdu::UnitResponse(UnitResponse {}));

        let registered_client = handler
            .client_id
            .as_ref()
            .expect("SetClientId must retain the exact registration");
        assert_eq!(registered_client.as_ref(), &client);
        assert!(mux.client_registration_is_current(registered_client));
        assert_eq!(
            mux.active_workspace_for_client(registered_client),
            "remote-dev",
        );
    }

    #[test]
    fn get_codec_version_returns_version_info() {
        let (sender, captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        handler.process_one(DecodedPdu {
            serial: 600,
            pdu: Pdu::GetCodecVersion(codec::GetCodecVersion {}),
        });

        let resp = take_response(&captured);
        assert_eq!(resp.serial, 600);
        match resp.pdu {
            Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                codec_vers,
                version_string,
                executable_path,
                ..
            }) => {
                assert_eq!(codec_vers, CODEC_VERSION);
                assert!(
                    !version_string.is_empty(),
                    "version string should not be empty"
                );
                assert!(
                    !executable_path.to_string_lossy().is_empty(),
                    "executable path should be non-empty"
                );
            }
            other => panic!("expected GetCodecVersionResponse, got {other:?}"),
        }
    }

    #[test]
    fn catch_helper_forwards_ok() {
        let (sender, captured) = capturing_sender();
        let serial = 700;
        let send_response = {
            let sender = sender.clone();
            move |result: anyhow::Result<Pdu>| {
                let pdu = match result {
                    Ok(pdu) => pdu,
                    Err(err) => Pdu::ErrorResponse(ErrorResponse {
                        reason: format!("{err:#}"),
                    }),
                };
                sender.send(DecodedPdu { pdu, serial }).ok();
            }
        };

        fn catch<F, SND>(f: F, send_response: SND)
        where
            F: FnOnce() -> anyhow::Result<Pdu>,
            SND: Fn(anyhow::Result<Pdu>),
        {
            send_response(f());
        }

        catch(|| Ok(Pdu::UnitResponse(UnitResponse {})), send_response);

        let resp = take_response(&captured);
        assert_eq!(resp.pdu, Pdu::UnitResponse(UnitResponse {}));
    }

    #[test]
    fn catch_helper_forwards_error() {
        let (sender, captured) = capturing_sender();
        let serial = 701;
        let send_response = {
            let sender = sender.clone();
            move |result: anyhow::Result<Pdu>| {
                let pdu = match result {
                    Ok(pdu) => pdu,
                    Err(err) => Pdu::ErrorResponse(ErrorResponse {
                        reason: format!("{err:#}"),
                    }),
                };
                sender.send(DecodedPdu { pdu, serial }).ok();
            }
        };

        fn catch<F, SND>(f: F, send_response: SND)
        where
            F: FnOnce() -> anyhow::Result<Pdu>,
            SND: Fn(anyhow::Result<Pdu>),
        {
            send_response(f());
        }

        catch(|| anyhow::bail!("something went wrong"), send_response);

        let resp = take_response(&captured);
        match resp.pdu {
            Pdu::ErrorResponse(ErrorResponse { reason }) => {
                assert!(
                    reason.contains("something went wrong"),
                    "error should propagate message, got: {reason}"
                );
            }
            other => panic!("expected ErrorResponse, got {other:?}"),
        }
    }

    #[test]
    fn multiple_per_pane_entries_are_independent() {
        let (sender, _captured) = capturing_sender();
        let mut handler = SessionHandler::new(sender);

        let pp1 = handler.per_pane(10);
        let pp2 = handler.per_pane(20);

        // Modify one, verify the other is unaffected
        pp1.lock().unwrap().seqno = 42;
        assert_eq!(pp2.lock().unwrap().seqno, 0);
    }

    #[test]
    fn maybe_push_pane_changes_drops_per_pane_lock_before_sending() {
        let pane: Arc<dyn Pane> = Arc::new(FakePane::new(None));
        let (_mux, registration) = register_test_pane(&pane);
        let per_pane = Arc::new(Mutex::new(PerPane::default()));
        let send_count = Arc::new(AtomicUsize::new(0));

        let sender = PduSender::new({
            let per_pane = Arc::clone(&per_pane);
            let send_count = Arc::clone(&send_count);
            move |_| {
                assert!(
                    per_pane.try_lock().is_ok(),
                    "PduSender callback must not run while per-pane state is locked"
                );
                send_count.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
        });

        registration
            .try_with_current(|current| maybe_push_pane_changes(&current, sender, per_pane))
            .expect("test pane registration remains current")
            .expect("pane changes should send");

        assert!(
            send_count.load(Ordering::Relaxed) >= 2,
            "initial render and palette PDUs should both be sent"
        );
    }

    #[test]
    fn compute_changes_includes_initial_tiered_scrollback_status_snapshot() {
        let pane = Arc::new(FakePane::new(Some(sample_tiered_scrollback_status(12))));
        let pane_dyn: Arc<dyn Pane> = pane;
        let (_mux, registration) = register_test_pane(&pane_dyn);
        let mut per_pane = PerPane::default();

        let response = registration
            .try_with_current(|current| per_pane.compute_changes(&current, None))
            .expect("test pane registration remains current")
            .expect("initial pane snapshot should produce a response");

        assert_eq!(
            response.tiered_scrollback_status,
            Some(sample_tiered_scrollback_status(12))
        );
        assert_eq!(
            per_pane.tiered_scrollback_status,
            Some(sample_tiered_scrollback_status(12))
        );
    }

    #[test]
    fn compute_changes_detects_cleared_tiered_scrollback_status_without_other_deltas() {
        let pane = Arc::new(FakePane::new(Some(sample_tiered_scrollback_status(12))));
        let pane_dyn: Arc<dyn Pane> = pane.clone();
        let (_mux, registration) = register_test_pane(&pane_dyn);
        let mut per_pane = PerPane::default();

        let initial = registration
            .try_with_current(|current| per_pane.compute_changes(&current, None))
            .expect("test pane registration remains current");
        assert!(
            initial.is_some(),
            "first snapshot should populate cached pane state"
        );
        assert!(
            registration
                .try_with_current(|current| per_pane.compute_changes(&current, None))
                .expect("test pane registration remains current")
                .is_none(),
            "unchanged pane state should not emit a redundant render delta"
        );

        pane.set_tiered_scrollback_status(None);

        let response = registration
            .try_with_current(|current| per_pane.compute_changes(&current, None))
            .expect("test pane registration remains current")
            .expect("clearing tiered scrollback status should produce a response");

        assert!(response.dirty_lines.is_empty());
        assert_eq!(response.tiered_scrollback_status, None);
        assert_eq!(per_pane.tiered_scrollback_status, None);
    }

    #[test]
    fn compute_changes_detects_alt_screen_transition_without_other_deltas() {
        let pane = Arc::new(FakePane::new(None));
        let pane_dyn: Arc<dyn Pane> = pane.clone();
        let (_mux, registration) = register_test_pane(&pane_dyn);
        let mut per_pane = PerPane::default();

        assert!(
            registration
                .try_with_current(|current| per_pane.compute_changes(&current, None))
                .expect("test pane registration remains current")
                .is_some(),
            "first snapshot should populate cached pane state"
        );
        assert!(
            registration
                .try_with_current(|current| per_pane.compute_changes(&current, None))
                .expect("test pane registration remains current")
                .is_none(),
            "unchanged pane state should not emit a redundant render delta"
        );

        pane.state.lock().unwrap().alt_screen_active = true;

        let response = registration
            .try_with_current(|current| per_pane.compute_changes(&current, None))
            .expect("test pane registration remains current")
            .expect("alt-screen transition should produce a response");

        assert!(response.alt_screen_active);
        assert!(per_pane.alt_screen_active);
    }

    #[test]
    fn compute_changes_skips_missing_cursor_row_in_bonus_lines() {
        let pane = Arc::new(FakePane::new(None));
        pane.state.lock().unwrap().cursor_position.y = 99;
        let pane_dyn: Arc<dyn Pane> = pane;
        let (_mux, registration) = register_test_pane(&pane_dyn);
        let mut per_pane = PerPane::default();

        let response = registration
            .try_with_current(|current| per_pane.compute_changes(&current, None))
            .expect("test pane registration remains current")
            .expect("initial pane snapshot should still produce a response");

        let cursor_y = response.cursor_position.y;
        let (bonus_lines, _images) = response.bonus_lines.extract_data();

        assert!(bonus_lines.is_empty());
        assert_eq!(cursor_y, 99);
    }

    #[test]
    fn compute_changes_deduplicates_dirty_cursor_row_in_bonus_lines() {
        let pane = Arc::new(FakePane::new(None));
        pane.set_changed_line(0);
        let pane_dyn: Arc<dyn Pane> = pane;
        let (_mux, registration) = register_test_pane(&pane_dyn);
        let mut per_pane = PerPane::default();

        let response = registration
            .try_with_current(|current| per_pane.compute_changes(&current, None))
            .expect("test pane registration remains current")
            .expect("dirty cursor row should produce a response");
        let (bonus_lines, _images) = response
            .bonus_lines
            .extract_data_checked()
            .expect("cursor row must appear exactly once");

        assert_eq!(bonus_lines.len(), 1);
        assert_eq!(bonus_lines[0].0, 0);
        assert!(response.dirty_lines.is_empty());
    }

    /// Regression: FakePane::writer() previously panicked via
    /// `unimplemented!()`. After br-ft-35yac.3 it returns a
    /// MappedMutexGuard over an `io::Sink`; bytes are discarded but
    /// the call no longer crashes.
    #[test]
    fn fake_pane_writer_returns_sink_instead_of_panicking() {
        let pane = FakePane::new(None);
        // Write through the trait object form to exercise the same
        // code path real callers use.
        let pane_dyn: &dyn Pane = &pane;
        {
            let mut writer = pane_dyn.writer();
            writer
                .write_all(b"discarded payload")
                .expect("io::sink never fails");
            writer.flush().expect("io::sink flush never fails");
        }
        // A second call must also succeed — the prior guard was
        // dropped above so the lock is released.
        {
            let mut writer = pane_dyn.writer();
            writer
                .write_all(b"second call")
                .expect("io::sink never fails");
        }
    }
}
