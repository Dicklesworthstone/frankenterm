use crate::activity::Activity;
use crate::domain::{alloc_domain_id, Domain, DomainId, DomainState, SplitSource};
use crate::pane::{Pane, PaneId};
use crate::tab::{SplitRequest, Tab, TabId};
use crate::tmux_commands::{ListAllPanes, ListAllWindows, ListCommands, SplitPane, TmuxCommand};
use crate::tmux_pty::TmuxChildState;
use crate::window::WindowId;
use crate::{Mux, MuxWindowBuilder};
use anyhow::Context;
use async_trait::async_trait;
use config::configuration;
use filedescriptor::FileDescriptor;
use frankenterm_term::{KeyCode, KeyModifiers, TerminalSize};
use parking_lot::Mutex;
use portable_pty::CommandBuilder;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::sync::Arc;
use termwiz::tmux_cc::*;

/// Returns the maximum backlog payload size per pane. Payloads exceeding
/// this are truncated to the tail bytes so the most recent output is
/// preserved. Read from config; defaults to 1 MiB.
fn max_backlog_bytes_per_pane() -> usize {
    configuration().mux_tmux_max_backlog_bytes_per_pane
}

/// Warning threshold for tmux command queue depth. Exceeding this
/// indicates protocol churn or a stalled consumer.
const CMD_QUEUE_WARNING_DEPTH: usize = 10_000;

/// Hard cap on tmux command queue size to prevent unbounded memory growth
/// during event storms. Oldest commands are dropped when the cap is reached.
const CMD_QUEUE_MAX_DEPTH: usize = 50_000;

fn cap_backlog_payload(payload: &[u8]) -> Vec<u8> {
    if payload.len() <= max_backlog_bytes_per_pane() {
        payload.to_vec()
    } else {
        log::warn!(
            "tmux backlog payload ({} bytes) exceeds {} limit; keeping tail",
            payload.len(),
            max_backlog_bytes_per_pane(),
        );
        payload[payload.len() - max_backlog_bytes_per_pane()..].to_vec()
    }
}

/// Push a command onto the tmux cmd_queue while enforcing the CMD_QUEUE_MAX_DEPTH
/// hard cap (ft-4qom2).
///
/// The cap was previously enforced only at the pop site (`send_next_command`),
/// which early-returns when `state != Idle`. If tmux is stuck in
/// `WaitingForResponse` during an event storm (remote tmux lag, server
/// overload), the pop-site enforcement never runs and the queue grows
/// unboundedly as LayoutChange/SessionChanged/WindowAdd/SplitPane events push
/// new commands without a cap check. Enforcing on every push closes that gap.
///
/// Caller holds the cmd_queue lock, so this is atomic with the push.
fn push_command_capped(queue: &mut TmuxCmdQueue, cmd: Box<dyn TmuxCommand>) {
    queue.push_back(cmd);
    if queue.len() > CMD_QUEUE_MAX_DEPTH {
        let excess = queue.len() - CMD_QUEUE_MAX_DEPTH;
        log::error!(
            "tmux command queue ({}) exceeds hard cap {}; dropping {} oldest commands",
            queue.len(),
            CMD_QUEUE_MAX_DEPTH,
            excess,
        );
        queue.drain(..excess);
    } else if queue.len() > CMD_QUEUE_WARNING_DEPTH {
        log::warn!(
            "tmux command queue depth ({}) exceeds {} threshold; possible protocol churn",
            queue.len(),
            CMD_QUEUE_WARNING_DEPTH,
        );
    }
}

#[derive(PartialEq, Eq, Debug, Copy, Clone)]
pub enum AttachState {
    Init,
    Done,
}

#[derive(PartialEq, Eq, Debug, Copy, Clone)]
enum State {
    WaitForInitialGuard,
    Idle,
    WaitingForResponse,
    Exit,
}

#[allow(dead_code)]
#[derive(Debug)]
pub(crate) struct TmuxRemotePane {
    // members for local
    pub local_pane_id: PaneId,
    pub output_write: FileDescriptor,
    pub child_state: Arc<TmuxChildState>,
    // members sync with remote
    pub session_id: TmuxSessionId,
    pub window_id: TmuxWindowId,
    pub pane_id: TmuxPaneId,
    pub cursor_x: u64,
    pub cursor_y: u64,
    pub pane_width: u64,
    pub pane_height: u64,
    pub pane_left: u64,
    pub pane_top: u64,
}

pub(crate) type RefTmuxRemotePane = Arc<Mutex<TmuxRemotePane>>;

/// As a remote TmuxTab, keeping the TmuxPanes ID
/// within the remote tab.
#[allow(dead_code)]
pub(crate) struct TmuxTab {
    pub tab_id: TabId, // local tab ID
    pub tmux_window_id: TmuxWindowId,
    pub layout_csum: String,
    pub panes: HashSet<TmuxPaneId>, // tmux panes within tmux window
}

pub(crate) type TmuxCmdQueue = VecDeque<Box<dyn TmuxCommand>>;
pub(crate) struct TmuxDomainState {
    pub pane_id: PaneId,     // ID of the original pane
    pub domain_id: DomainId, // ID of TmuxDomain
    state: Mutex<State>,
    pub cmd_queue: Arc<Mutex<TmuxCmdQueue>>,
    pub gui_window: Mutex<Option<MuxWindowBuilder>>,
    pub gui_tabs: Mutex<HashMap<TmuxWindowId, TmuxTab>>,
    pub remote_panes: Mutex<HashMap<TmuxPaneId, RefTmuxRemotePane>>,
    pub tmux_session: Mutex<Option<TmuxSessionId>>,
    pub support_commands: Mutex<HashMap<String, String>>,
    pub attach_state: Mutex<AttachState>,
    pub notification_sub_id: Mutex<Option<usize>>,
    pending_splits: Mutex<VecDeque<promise::Promise<TmuxPaneId>>>,
    pub backlog: Mutex<HashMap<TmuxPaneId, Vec<u8>>>,
}

pub struct TmuxDomain {
    pub(crate) inner: Arc<TmuxDomainState>,
}

impl TmuxDomainState {
    pub fn advance(&self, events: Box<Vec<Event>>) {
        for event in events.iter() {
            let state = *self.state.lock();
            log::debug!("tmux: {:?} in state {:?}", event, state);
            match event {
                // Tmux generic events
                Event::Guarded(response) => match state {
                    State::WaitForInitialGuard => {
                        *self.state.lock() = State::Idle;
                    }
                    State::WaitingForResponse => {
                        let mut cmd_queue = self.cmd_queue.as_ref().lock();
                        if let Some(cmd) = cmd_queue.pop_front() {
                            let domain_id = self.domain_id;
                            *self.state.lock() = State::Idle;
                            let resp = response.clone();
                            promise::spawn::spawn_into_main_thread(async move {
                                if let Err(err) = cmd.process_result(domain_id, &resp) {
                                    log::error!("Tmux processing command result error: {}", err);
                                }
                            })
                            .detach();
                        }
                    }
                    State::Idle => {}
                    State::Exit => {}
                },

                // Tmux specific events
                Event::ConfigError { error } => {
                    // tmux config file error, not our fault, just log it and go
                    log::warn!("tmux configuration error: {error}");
                }
                Event::Exit { reason: _ } => {
                    *self.state.lock() = State::Exit;
                    self.unsubscribe_notification();

                    {
                        let mut pane_map = self.remote_panes.lock();
                        for (_, v) in pane_map.iter_mut() {
                            let remote_pane = v.lock();
                            remote_pane
                                .child_state
                                .mark_exited(portable_pty::ExitStatus::with_exit_code(0));
                        }
                        // Drop remote pane state as soon as the tmux session exits.
                        pane_map.clear();
                    }

                    self.backlog.lock().clear();
                    self.gui_tabs.lock().clear();
                    self.pending_splits.lock().clear();
                    self.tmux_session.lock().take();

                    let mut cmd_queue = self.cmd_queue.as_ref().lock();
                    cmd_queue.clear();

                    // Force-exit tmux mode in the launcher pane and then
                    // remove all panes in this detached domain from mux state.
                    let domain_id = self.domain_id;
                    let pane_id = self.pane_id;
                    promise::spawn::spawn_into_main_thread_with_low_priority(async move {
                        if let Some(mux) = Mux::try_get() {
                            if let Some(x) = mux.get_pane(pane_id) {
                                let _ = write!(x.writer(), "\n\n");
                            }
                            mux.domain_was_detached(domain_id);
                        }
                    })
                    .detach();

                    return;
                }
                Event::LayoutChange {
                    window,
                    layout,
                    visible_layout: _,
                    raw_flags: _,
                } => {
                    let mut cmd_queue = self.cmd_queue.as_ref().lock();
                    push_command_capped(
                        &mut cmd_queue,
                        Box::new(ListAllPanes {
                            window_id: *window,
                            prune: true,
                            layout_csum: if let Some(l) = layout.get(0..4) {
                                l.to_string()
                            } else {
                                "".to_string()
                            },
                        }),
                    );
                }
                Event::Output { pane, text } => {
                    let pane_map = self.remote_panes.lock();
                    if let Some(ref_pane) = pane_map.get(pane) {
                        let mut tmux_pane = ref_pane.lock();
                        if let Err(err) = tmux_pane.output_write.write_all(text) {
                            log::error!("Failed to write tmux data to output: {:#}", err);
                        }
                    } else {
                        // the output may come early then pane is ready, in this case we
                        // backlog it
                        self.backlog.lock().insert(*pane, cap_backlog_payload(text));
                        log::debug!("Tmux pane {} havn't been attached", pane);
                    }
                }
                Event::SessionChanged { session, name: _ } => {
                    *self.tmux_session.lock() = Some(*session);
                    let mut cmd_queue = self.cmd_queue.as_ref().lock();
                    push_command_capped(&mut cmd_queue, Box::new(ListCommands));

                    self.subscribe_notification();
                    log::info!("tmux session changed:{}", session);
                }
                Event::WindowAdd { window } => {
                    // Only handle the new tab, the first empty window handled by sync_window_state
                    if let (true, Some(session)) =
                        (self.gui_window.lock().is_some(), *self.tmux_session.lock())
                    {
                        let mut cmd_queue = self.cmd_queue.as_ref().lock();
                        push_command_capped(
                            &mut cmd_queue,
                            Box::new(ListAllWindows {
                                session_id: session,
                                window_id: Some(*window),
                            }),
                        );
                        log::info!("tmux window add: {}:{}", session, window);
                    }
                }
                Event::WindowClose { window } => {
                    let _ = self.remove_detached_window(*window);
                }
                Event::WindowPaneChanged { window, pane } => {
                    // The tmux 2.7 WindowPaneChanged event comes early than WindowAdd, we need to
                    // skip it
                    if !self.check_window_attached(*window) {
                        continue;
                    }

                    // Split pane
                    if !self.check_pane_attached(*window, *pane) {
                        let mut pending_splits = self.pending_splits.lock();
                        if let Some(mut promise) = pending_splits.pop_front() {
                            promise.ok(*pane);
                        }
                    }
                    log::info!("tmux window pane changed: {}:{}", window, pane);
                }
                Event::WindowRenamed { window, name } => {
                    let gui_tabs = self.gui_tabs.lock();
                    if let Some(x) = gui_tabs.get(&window) {
                        let mux = Mux::get();
                        if let Some(tab) = mux.get_tab(x.tab_id) {
                            tab.set_title(&format!("{}", name));
                        }
                    }
                }
                Event::UnlinkedWindowClose { window } => {
                    let _ = self.remove_detached_window(*window);
                }
                _ => {}
            }
        }

        // send pending commands to tmux
        let cmd_queue = self.cmd_queue.as_ref().lock();
        if *self.state.lock() == State::Idle && !cmd_queue.is_empty() {
            TmuxDomainState::schedule_send_next_command(self.domain_id);
        }
    }

    /// send next command at the front of cmd_queue.
    /// must be called inside main thread
    fn send_next_command(&self) {
        if *self.state.lock() != State::Idle {
            return;
        }
        let mut cmd_queue = self.cmd_queue.as_ref().lock();
        if cmd_queue.len() > CMD_QUEUE_MAX_DEPTH {
            let excess = cmd_queue.len() - CMD_QUEUE_MAX_DEPTH;
            log::error!(
                "tmux command queue ({}) exceeds hard cap {}; dropping {} oldest commands",
                cmd_queue.len(),
                CMD_QUEUE_MAX_DEPTH,
                excess,
            );
            cmd_queue.drain(..excess);
        } else if cmd_queue.len() > CMD_QUEUE_WARNING_DEPTH {
            log::warn!(
                "tmux command queue depth ({}) exceeds {} threshold; possible protocol churn",
                cmd_queue.len(),
                CMD_QUEUE_WARNING_DEPTH,
            );
        }
        while let Some(first) = cmd_queue.front() {
            let cmd = first.get_command(self.domain_id);
            if cmd.is_empty() {
                cmd_queue.pop_front();
                continue;
            }
            log::debug!("sending cmd {:?}", cmd);
            let mux = Mux::get();
            if let Some(pane) = mux.get_pane(self.pane_id) {
                let mut writer = pane.writer();
                let _ = write!(writer, "{}", cmd);
            }
            *self.state.lock() = State::WaitingForResponse;
            break;
        }
    }

    /// schedule a `send_next_command` into main thread
    pub fn schedule_send_next_command(domain_id: usize) {
        promise::spawn::spawn_into_main_thread(async move {
            let mux = Mux::get();
            if let Some(domain) = mux.get_domain(domain_id) {
                if let Some(tmux_domain) = domain.downcast_ref::<TmuxDomain>() {
                    tmux_domain.send_next_command();
                }
            }
        })
        .detach();
    }

    /// create a standalone window for tmux tabs
    pub fn create_gui_window(&self) {
        if self.gui_window.lock().is_none() {
            let mux = Mux::get();
            let window_builder =
                if let Some((_domain, window_id, _tab)) = mux.resolve_pane_id(self.pane_id) {
                    MuxWindowBuilder {
                        window_id,
                        activity: Some(Activity::new()),
                        notified: false,
                    }
                } else {
                    mux.new_empty_window(Some("tmux".to_string()), None /* position */)
                };

            log::info!("Tmux create window id {}", window_builder.window_id);
            {
                let mut window_id = self.gui_window.lock();
                *window_id = Some(window_builder); // keep the builder so it won't be purged
            }
        };
    }

    /// split the tmux pane
    pub fn split_tmux_pane(
        &self,
        _tab: TabId,
        pane_id: PaneId,
        split_request: SplitRequest,
    ) -> anyhow::Result<()> {
        let tmux_pane_id = self
            .remote_panes
            .lock()
            .iter()
            .find(|(_, ref_pane)| ref_pane.lock().local_pane_id == pane_id)
            .map(|p| p.1.lock().pane_id);

        if let Some(id) = tmux_pane_id {
            let mut cmd_queue = self.cmd_queue.as_ref().lock();
            push_command_capped(
                &mut cmd_queue,
                Box::new(SplitPane {
                    pane_id: id,
                    direction: split_request.direction,
                }),
            );
            TmuxDomainState::schedule_send_next_command(self.domain_id);
            return Ok(());
        } else {
            anyhow::bail!("Could not find the tmux pane peer for local pane: {pane_id}");
        }
    }
}

impl TmuxDomain {
    pub fn new(pane_id: PaneId) -> Self {
        let domain_id = alloc_domain_id();
        let cmd_queue = VecDeque::new();
        let inner = Arc::new(TmuxDomainState {
            domain_id,
            pane_id,
            // parser,
            state: Mutex::new(State::WaitForInitialGuard),
            cmd_queue: Arc::new(Mutex::new(cmd_queue)),
            gui_window: Mutex::new(None),
            gui_tabs: Mutex::new(HashMap::default()),
            remote_panes: Mutex::new(HashMap::default()),
            tmux_session: Mutex::new(None),
            support_commands: Mutex::new(HashMap::default()),
            attach_state: Mutex::new(AttachState::Init),
            notification_sub_id: Mutex::new(None),
            pending_splits: Mutex::new(VecDeque::default()),
            backlog: Mutex::new(HashMap::default()),
        });

        Self { inner }
    }

    fn spawn_unsupported(surface: &str) -> anyhow::Error {
        anyhow::anyhow!(
            "{surface} is unsupported for TmuxDomain because tmux control-mode windows and panes \
             materialize asynchronously from tmux events rather than returning an immediate local handle"
        )
    }

    fn send_next_command(&self) {
        self.inner.send_next_command();
    }
}

#[async_trait(?Send)]
impl Domain for TmuxDomain {
    async fn spawn(
        &self,
        _size: TerminalSize,
        _command: Option<CommandBuilder>,
        _command_dir: Option<String>,
        _window: WindowId,
    ) -> anyhow::Result<Arc<Tab>> {
        Err(Self::spawn_unsupported("spawn"))
    }

    async fn split_pane(
        &self,
        _source: SplitSource,
        tab: TabId,
        pane_id: PaneId,
        split_request: SplitRequest,
    ) -> anyhow::Result<Arc<dyn Pane>> {
        let mut promise = promise::Promise::new();
        if let Some(future) = promise.get_future() {
            {
                let mut pending_splits = self.inner.pending_splits.lock();
                let _ = self.inner.split_tmux_pane(tab, pane_id, split_request)?;
                pending_splits.push_back(promise);
            }

            if let Ok(id) = future.await {
                let pane = self.inner.split_pane(tab, pane_id, id, split_request);
                return pane;
            }
        }

        anyhow::bail!("Split_pane failed");
    }

    async fn spawn_pane(
        &self,
        _size: TerminalSize,
        _command: Option<CommandBuilder>,
        _command_dir: Option<String>,
    ) -> anyhow::Result<Arc<dyn Pane>> {
        Err(Self::spawn_unsupported("spawn_pane"))
    }

    fn domain_id(&self) -> DomainId {
        self.inner.domain_id
    }

    fn domain_name(&self) -> &str {
        "tmux"
    }

    async fn attach(&self, _window_id: Option<crate::WindowId>) -> anyhow::Result<()> {
        // Control-mode startup is bootstrapped by SessionChanged events rather
        // than an explicit attach command.
        Ok(())
    }

    fn detachable(&self) -> bool {
        true
    }

    fn detach(&self) -> anyhow::Result<()> {
        if *self.inner.state.lock() == State::Exit {
            return Ok(());
        }

        let mux = Mux::get();
        let pane = mux.get_pane(self.inner.pane_id).ok_or_else(|| {
            anyhow::anyhow!(
                "detach is unavailable for TmuxDomain because its launcher pane {} is gone",
                self.inner.pane_id
            )
        })?;

        pane.key_down(KeyCode::Char('q'), KeyModifiers::NONE)
            .context("sending detach key to tmux launcher pane")
    }

    fn state(&self) -> DomainState {
        match *self.inner.state.lock() {
            State::Exit => DomainState::Detached,
            _ => DomainState::Attached,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Domain, LocalDomain};
    use crate::pane::{CachePolicy, ForEachPaneLogicalLine, LogicalLine, Pane, WithPaneLines};
    use crate::renderable::{RenderableDimensions, StableCursorPosition};
    use frankenterm_term::color::ColorPalette;
    use parking_lot::{MappedMutexGuard, MutexGuard};
    use promise::spawn::block_on;
    use rangeset::RangeSet;
    use std::ops::Range;
    use std::sync::{Arc, Mutex as StdMutex, MutexGuard as StdMutexGuard, OnceLock};
    use termwiz::surface::{Line, SEQ_ZERO};
    use url::Url;

    fn mux_test_lock() -> &'static StdMutex<()> {
        static LOCK: OnceLock<StdMutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| StdMutex::new(()))
    }

    struct ScopedMux {
        prior: Option<Arc<Mux>>,
        _guard: StdMutexGuard<'static, ()>,
    }

    impl ScopedMux {
        fn install(mux: Arc<Mux>) -> Self {
            let guard = mux_test_lock()
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            let prior = Mux::try_get();
            Mux::set_mux(&mux);
            Self {
                prior,
                _guard: guard,
            }
        }
    }

    impl Drop for ScopedMux {
        fn drop(&mut self) {
            if let Some(prior) = self.prior.take() {
                Mux::set_mux(&prior);
            } else {
                Mux::shutdown();
            }
        }
    }

    struct RecordingPane {
        pane_id: PaneId,
        domain_id: DomainId,
        keys: Mutex<Vec<char>>,
        writes: Mutex<Vec<u8>>,
    }

    impl RecordingPane {
        fn new(pane_id: PaneId, domain_id: DomainId) -> Arc<Self> {
            Arc::new(Self {
                pane_id,
                domain_id,
                keys: Mutex::new(Vec::new()),
                writes: Mutex::new(Vec::new()),
            })
        }

        fn recorded_keys(&self) -> Vec<char> {
            self.keys.lock().clone()
        }

        fn recorded_writes(&self) -> Vec<u8> {
            self.writes.lock().clone()
        }
    }

    impl Pane for RecordingPane {
        fn pane_id(&self) -> PaneId {
            self.pane_id
        }

        fn get_cursor_position(&self) -> StableCursorPosition {
            StableCursorPosition::default()
        }

        fn get_current_seqno(&self) -> termwiz::surface::SequenceNo {
            SEQ_ZERO
        }

        fn get_changed_since(
            &self,
            _lines: Range<frankenterm_term::StableRowIndex>,
            _seqno: termwiz::surface::SequenceNo,
        ) -> RangeSet<frankenterm_term::StableRowIndex> {
            RangeSet::new()
        }

        fn get_lines(
            &self,
            _lines: Range<frankenterm_term::StableRowIndex>,
        ) -> (frankenterm_term::StableRowIndex, Vec<Line>) {
            (0, Vec::new())
        }

        fn with_lines_mut(
            &self,
            _lines: Range<frankenterm_term::StableRowIndex>,
            _with_lines: &mut dyn WithPaneLines,
        ) {
        }

        fn for_each_logical_line_in_stable_range_mut(
            &self,
            _lines: Range<frankenterm_term::StableRowIndex>,
            _for_line: &mut dyn ForEachPaneLogicalLine,
        ) {
        }

        fn get_logical_lines(
            &self,
            _lines: Range<frankenterm_term::StableRowIndex>,
        ) -> Vec<LogicalLine> {
            Vec::new()
        }

        fn get_dimensions(&self) -> RenderableDimensions {
            RenderableDimensions {
                cols: 80,
                viewport_rows: 24,
                scrollback_rows: 24,
                physical_top: 0,
                scrollback_top: 0,
                dpi: 0,
                pixel_width: 0,
                pixel_height: 0,
                reverse_video: false,
            }
        }

        fn get_title(&self) -> String {
            "recording-pane".to_string()
        }

        fn send_paste(&self, _text: &str) -> anyhow::Result<()> {
            Ok(())
        }

        fn reader(&self) -> anyhow::Result<Option<Box<dyn std::io::Read + Send>>> {
            Ok(None)
        }

        fn writer(&self) -> MappedMutexGuard<'_, dyn std::io::Write> {
            MutexGuard::map(self.writes.lock(), |writes| {
                let writer: &mut dyn std::io::Write = writes;
                writer
            })
        }

        fn resize(&self, _size: TerminalSize) -> anyhow::Result<()> {
            Ok(())
        }

        fn key_down(&self, key: KeyCode, _mods: KeyModifiers) -> anyhow::Result<()> {
            self.keys.lock().push(match key {
                KeyCode::Char(c) => c,
                _ => '\0',
            });
            Ok(())
        }

        fn key_up(&self, _key: KeyCode, _mods: KeyModifiers) -> anyhow::Result<()> {
            Ok(())
        }

        fn mouse_event(&self, _event: frankenterm_term::MouseEvent) -> anyhow::Result<()> {
            Ok(())
        }

        fn is_dead(&self) -> bool {
            false
        }

        fn palette(&self) -> ColorPalette {
            ColorPalette::default()
        }

        fn domain_id(&self) -> DomainId {
            self.domain_id
        }

        fn is_mouse_grabbed(&self) -> bool {
            false
        }

        fn is_alt_screen_active(&self) -> bool {
            false
        }

        fn get_current_working_dir(&self, _policy: CachePolicy) -> Option<Url> {
            None
        }
    }

    #[test]
    fn backlog_payload_cap_keeps_small_payload_unchanged() {
        let small = vec![0u8; 1024];
        let capped = cap_backlog_payload(&small);
        assert_eq!(small, capped);
    }

    #[test]
    fn tmux_domain_detach_sends_detach_key_to_launcher_pane() {
        let default_domain: Arc<dyn Domain> =
            Arc::new(LocalDomain::new("tmux-detach-default").expect("local domain"));
        let mux = Arc::new(Mux::new(Some(Arc::clone(&default_domain))));
        let _guard = ScopedMux::install(Arc::clone(&mux));

        let launcher = RecordingPane::new(77, default_domain.domain_id());
        let launcher_dyn: Arc<dyn Pane> = launcher.clone();
        mux.add_pane(&launcher_dyn).expect("add launcher pane");

        let tmux_domain = TmuxDomain::new(77);
        assert!(tmux_domain.detachable());
        tmux_domain.detach().expect("detach tmux domain");

        assert_eq!(launcher.recorded_keys(), vec!['q']);
    }

    #[test]
    fn tmux_domain_detach_requires_launcher_pane() {
        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(mux);

        let tmux_domain = TmuxDomain::new(1234);
        let err = tmux_domain
            .detach()
            .expect_err("detach should fail without launcher pane");
        let err = err.to_string();
        assert!(err.contains("launcher pane"), "{}", err);
        assert!(err.contains("TmuxDomain"), "{}", err);
    }

    #[test]
    fn tmux_domain_spawn_is_explicitly_unsupported_without_queueing_side_effects() {
        let tmux_domain = TmuxDomain::new(77);
        let err = match block_on(tmux_domain.spawn(TerminalSize::default(), None, None, 0)) {
            Ok(_) => panic!("tmux spawn should be unsupported"),
            Err(err) => err,
        };
        let err = err.to_string();
        assert!(err.contains("unsupported"), "{}", err);
        assert!(err.contains("TmuxDomain"), "{}", err);
        assert!(tmux_domain.inner.cmd_queue.lock().is_empty());
    }

    #[test]
    fn tmux_domain_spawn_pane_is_explicitly_unsupported_without_queueing_side_effects() {
        let tmux_domain = TmuxDomain::new(77);
        let err = match block_on(tmux_domain.spawn_pane(TerminalSize::default(), None, None)) {
            Ok(_) => panic!("tmux spawn_pane should be unsupported"),
            Err(err) => err,
        };
        let err = err.to_string();
        assert!(err.contains("unsupported"), "{}", err);
        assert!(err.contains("TmuxDomain"), "{}", err);
        assert!(tmux_domain.inner.cmd_queue.lock().is_empty());
    }

    #[test]
    fn tmux_recording_pane_writer_captures_bytes() {
        let pane = RecordingPane::new(88, 0);
        pane.writer()
            .write_all(b"detach\n")
            .expect("write recording pane bytes");
        assert_eq!(pane.recorded_writes(), b"detach\n".to_vec());
    }

    #[test]
    fn tmux_domain_state_reports_detached_after_exit() {
        let tmux_domain = TmuxDomain::new(0);
        *tmux_domain.inner.state.lock() = State::Exit;
        assert_eq!(tmux_domain.state(), DomainState::Detached);
    }

    #[test]
    fn backlog_payload_cap_keeps_tail_when_payload_exceeds_limit() {
        let large_len = max_backlog_bytes_per_pane() + 512;
        let large: Vec<u8> = (0..large_len).map(|i| (i % 256) as u8).collect();
        let capped = cap_backlog_payload(&large);
        assert_eq!(max_backlog_bytes_per_pane(), capped.len());
        // The capped output should be the tail of the original
        assert_eq!(
            &large[large_len - max_backlog_bytes_per_pane()..],
            &capped[..]
        );
    }

    #[test]
    fn backlog_payload_cap_at_exact_limit_is_unchanged() {
        let exact = vec![42u8; max_backlog_bytes_per_pane()];
        let capped = cap_backlog_payload(&exact);
        assert_eq!(exact.len(), capped.len());
        assert_eq!(exact, capped);
    }

    #[test]
    fn cmd_queue_hard_cap_drains_oldest_when_exceeded() {
        let default_domain: Arc<dyn Domain> =
            Arc::new(LocalDomain::new("tmux-queue-cap-default").expect("local domain"));
        let mux = Arc::new(Mux::new(Some(Arc::clone(&default_domain))));
        let _guard = ScopedMux::install(Arc::clone(&mux));

        let launcher = RecordingPane::new(99, default_domain.domain_id());
        let launcher_dyn: Arc<dyn Pane> = launcher.clone();
        mux.add_pane(&launcher_dyn).expect("add launcher pane");

        let tmux_domain = TmuxDomain::new(99);
        // Set state to Idle so send_next_command processes the queue
        *tmux_domain.inner.state.lock() = State::Idle;

        // Fill the queue beyond the hard cap
        {
            let mut queue = tmux_domain.inner.cmd_queue.lock();
            for _ in 0..CMD_QUEUE_MAX_DEPTH + 100 {
                queue.push_back(Box::new(ListCommands));
            }
            assert_eq!(queue.len(), CMD_QUEUE_MAX_DEPTH + 100);
        }

        // send_next_command should drain excess before processing
        tmux_domain.inner.send_next_command();

        let queue = tmux_domain.inner.cmd_queue.lock();
        // Queue should be at or below the cap (minus the one that was sent)
        assert!(
            queue.len() <= CMD_QUEUE_MAX_DEPTH,
            "queue length {} should be at most {}",
            queue.len(),
            CMD_QUEUE_MAX_DEPTH,
        );
    }
}
