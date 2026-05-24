use crate::domain::{DomainId, WriterWrapper};
use crate::localpane::LocalPane;
use crate::pane::{PaneId, alloc_pane_id};
use crate::tab::{SplitDirection, SplitRequest, SplitSize, Tab, TabId};
use crate::tmux::{AttachState, TmuxDomain, TmuxDomainState, TmuxRemotePane, TmuxTab};
use crate::tmux_pty::{TmuxChild, TmuxChildState, TmuxPty};
use crate::{Mux, MuxNotification, Pane};
use anyhow::{Context, anyhow};
use frankenterm_term::TerminalSize;
use parking_lot::Mutex;
use portable_pty::{ExitStatus, MasterPty, PtySize};
use std::collections::{HashMap, HashSet};
use std::fmt::{Debug, Write};
use std::io::Write as _;
use std::sync::Arc;
use termwiz::escape::csi::{CSI, Cursor};
use termwiz::escape::{Action, OneBased};
use termwiz::tmux_cc::*;

pub(crate) trait TmuxCommand: Send + Debug {
    fn get_command(&self, domain_id: DomainId) -> String;
    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()>;
}

fn tmux_mux() -> anyhow::Result<Arc<Mux>> {
    Mux::try_get().ok_or_else(|| anyhow!("tmux command requires active mux"))
}

fn u64_to_usize_saturating(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

fn usize_to_u32_saturating(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn next_split_pane_index(inserted_index: usize) -> usize {
    inserted_index.saturating_add(1)
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PaneItem {
    session_id: TmuxSessionId,
    window_id: TmuxWindowId,
    pane_id: TmuxPaneId,
    _pane_index: u64,
    cursor_x: u64,
    cursor_y: u64,
    pane_width: u64,
    pane_height: u64,
    pane_left: u64,
    pane_top: u64,
    pane_active: bool,
}

#[derive(Debug)]
struct WindowItem {
    session_id: TmuxSessionId,
    window_id: TmuxWindowId,
    window_width: u64,
    window_height: u64,
    window_active: bool,
    window_name: String,
    layout: Vec<WindowLayout>,
    layout_csum: String,
    history_limit: isize,
}

impl TmuxDomainState {
    /// check if a PaneItem received from ListAllPanes has been attached
    pub fn check_pane_attached(&self, window_id: TmuxWindowId, pane_id: TmuxPaneId) -> bool {
        let gui_tabs = self.gui_tabs.lock();
        let Some(local_tab) = gui_tabs.get(&window_id) else {
            return false;
        };

        return local_tab.panes.get(&pane_id).is_some();
    }

    pub fn check_window_attached(&self, window_id: TmuxWindowId) -> bool {
        let gui_tabs = self.gui_tabs.lock();
        return gui_tabs.get(&window_id).is_some();
    }

    /// after we create a tab for a remote pane, save its ID into the
    /// TmuxPane-TmuxPane tree, so we can ref it later.
    fn add_attached_pane(
        &self,
        window_id: TmuxWindowId,
        pane_id: TmuxPaneId,
    ) -> anyhow::Result<()> {
        let mut gui_tabs = self.gui_tabs.lock();

        let panes = match gui_tabs.get_mut(&window_id) {
            Some(tab) => &mut tab.panes,
            None => anyhow::bail!("The window {window_id} is not attached"),
        };

        match panes.get(&pane_id) {
            Some(_) => {
                anyhow::bail!("Tmux pane already attached");
            }
            None => {
                panes.insert(pane_id);
                return Ok(());
            }
        }
    }

    fn add_attached_window(&self, target: &WindowItem, tab_id: &TabId) -> anyhow::Result<()> {
        let mut gui_tabs = self.gui_tabs.lock();
        if !gui_tabs.contains_key(&target.window_id) {
            gui_tabs.insert(
                target.window_id,
                TmuxTab {
                    tab_id: *tab_id,
                    tmux_window_id: target.window_id,
                    layout_csum: target.layout_csum.clone(),
                    panes: HashSet::new(),
                },
            );
        }

        Ok(())
    }

    fn remove_tmux_pane_state_entries(
        remote_panes: &mut HashMap<TmuxPaneId, Arc<Mutex<TmuxRemotePane>>>,
        backlog: &mut HashMap<TmuxPaneId, Vec<u8>>,
        pane_ids: &[TmuxPaneId],
    ) -> Vec<PaneId> {
        let mut local_pane_ids = Vec::with_capacity(pane_ids.len());
        for pane_id in pane_ids {
            if let Some(remote) = remote_panes.remove(pane_id) {
                let remote = remote.lock();
                remote
                    .child_state
                    .mark_exited(ExitStatus::with_exit_code(0));
                local_pane_ids.push(remote.local_pane_id);
            }
            let _ = backlog.remove(pane_id);
        }
        local_pane_ids
    }

    fn remove_detached_pane(
        &self,
        window_id: TmuxWindowId,
        new_set: &HashSet<TmuxPaneId>,
    ) -> anyhow::Result<()> {
        let (tab_id, to_remove, tab_empty) = {
            let mut gui_tabs = self.gui_tabs.lock();

            let (tab_id, panes) = match gui_tabs.get_mut(&window_id) {
                Some(tab) => (tab.tab_id, &mut tab.panes),
                None => anyhow::bail!("The window {window_id} is not attached"),
            };

            let to_remove: Vec<_> = panes.difference(new_set).cloned().collect();
            for pane_id in &to_remove {
                let _ = panes.remove(pane_id);
            }

            let tab_empty = panes.is_empty();
            if tab_empty {
                gui_tabs.remove(&window_id);
            }

            (tab_id, to_remove, tab_empty)
        };

        let local_pane_ids = {
            let mut pane_map = self.remote_panes.lock();
            let mut backlog = self.backlog.lock();
            Self::remove_tmux_pane_state_entries(&mut pane_map, &mut backlog, &to_remove)
        };

        let mux = tmux_mux()?;
        for pane_id in local_pane_ids {
            mux.remove_pane(pane_id);
        }
        if tab_empty {
            mux.remove_tab(tab_id);
        }

        Ok(())
    }

    pub fn remove_detached_window(&self, window_id: TmuxWindowId) -> anyhow::Result<()> {
        let tab = {
            let mut gui_tabs = self.gui_tabs.lock();
            match gui_tabs.remove(&window_id) {
                Some(tab) => tab,
                None => anyhow::bail!("Cannot find the window {window_id}"),
            }
        };

        let detached_panes: Vec<_> = tab.panes.iter().copied().collect();
        let local_pane_ids = {
            let mut pane_map = self.remote_panes.lock();
            let mut backlog = self.backlog.lock();
            Self::remove_tmux_pane_state_entries(&mut pane_map, &mut backlog, &detached_panes)
        };

        let mux = tmux_mux()?;
        for pane_id in local_pane_ids {
            mux.remove_pane(pane_id);
        }
        mux.remove_tab(tab.tab_id);

        Ok(())
    }

    fn set_pane_cursor_position(&self, pane: &Arc<dyn Pane>, x: usize, y: usize) {
        pane.perform_actions(vec![Action::CSI(CSI::Cursor(
            Cursor::CharacterAndLinePosition {
                col: OneBased::from_zero_based(usize_to_u32_saturating(x)),
                line: OneBased::from_zero_based(usize_to_u32_saturating(y)),
            },
        ))]);
    }

    fn create_pane(&self, pane: &PaneItem) -> anyhow::Result<Arc<dyn Pane>> {
        let local_pane_id = alloc_pane_id();
        let child_state = Arc::new(TmuxChildState::new());
        let (output_read, output_write) = filedescriptor::socketpair()?;
        let ref_pane = Arc::new(Mutex::new(TmuxRemotePane {
            local_pane_id,
            output_write,
            child_state: child_state.clone(),
            session_id: 0,
            window_id: pane.window_id,
            pane_id: pane.pane_id,
            cursor_x: pane.cursor_x,
            cursor_y: pane.cursor_y,
            pane_width: pane.pane_width,
            pane_height: pane.pane_height,
            pane_left: pane.pane_left,
            pane_top: pane.pane_top,
        }));

        {
            let mut pane_map = self.remote_panes.lock();
            pane_map.insert(pane.pane_id, ref_pane.clone());
        }

        let pane_pty = TmuxPty {
            domain_id: self.domain_id,
            reader: output_read,
            cmd_queue: self.cmd_queue.clone(),
            master_pane: ref_pane,
        };

        let writer = WriterWrapper::new(pane_pty.take_writer()?);

        let size = TerminalSize {
            rows: u64_to_usize_saturating(pane.pane_height),
            cols: u64_to_usize_saturating(pane.pane_width),
            pixel_width: 0,
            pixel_height: 0,
            dpi: 0,
        };

        let child = TmuxChild::new(
            self.domain_id,
            pane.pane_id,
            self.cmd_queue.clone(),
            child_state,
        );

        let command_description = "tmux pane".to_string();
        let term_config = config::TermConfig::new_for_pane(
            local_pane_id,
            self.domain_id,
            command_description.clone(),
        );
        let terminal = frankenterm_term::Terminal::new(
            size,
            std::sync::Arc::new(term_config),
            "WezTerm",
            config::wezterm_version(),
            Box::new(writer.clone()),
        );

        Ok(Arc::new(LocalPane::new(
            local_pane_id,
            terminal,
            Box::new(child),
            Box::new(pane_pty),
            Box::new(writer),
            self.domain_id,
            command_description,
        )))
    }

    pub fn split_pane(
        &self,
        tab_id: TabId,
        pane_id: PaneId,
        remote_id: TmuxPaneId,
        split_request: SplitRequest,
    ) -> anyhow::Result<Arc<dyn Pane>> {
        let mux = tmux_mux()?;
        let tab = match mux.get_tab(tab_id) {
            Some(t) => t,
            None => anyhow::bail!("Invalid tab id {}", tab_id),
        };

        let pane_index = match tab
            .iter_panes_ignoring_zoom()
            .iter()
            .find(|p| p.pane.pane_id() == pane_id)
        {
            Some(p) => p.index,
            None => anyhow::bail!("invalid pane id {}", pane_id),
        };

        let split_size = match tab.compute_split_size(pane_index, split_request) {
            Some(s) => s,
            None => anyhow::bail!("invalid pane index {}", pane_index),
        };

        let window_id = match self.gui_tabs.lock().iter().find(|t| t.1.tab_id == tab_id) {
            Some((_, tab)) => tab.tmux_window_id,
            None => anyhow::bail!("No tab {}", tab_id),
        };

        let p = PaneItem {
            session_id: 0,
            window_id: window_id,
            pane_id: remote_id,
            _pane_index: 0,
            cursor_x: 0,
            cursor_y: 0,
            pane_width: split_size.second.cols as u64,
            pane_height: split_size.second.rows as u64,
            pane_left: 0,
            pane_top: 0,
            pane_active: false,
        };

        let pane = self.create_pane(&p).context("failed to create pane")?;
        tab.split_and_insert(pane_index, split_request, Arc::clone(&pane))?;

        self.add_attached_pane(window_id, remote_id)?;

        let _ = mux.add_pane(&pane);

        return Ok(pane);
    }

    fn sync_pane_state(&self, panes: &[PaneItem]) -> anyhow::Result<()> {
        let Some(current_session) = *self.tmux_session.lock() else {
            return Ok(());
        };
        let mux = tmux_mux()?;

        for pane in panes.iter() {
            if pane.session_id != current_session
                || !self.check_pane_attached(pane.window_id, pane.pane_id)
            {
                continue;
            }

            // We now have the cursor information, fix the cursor position
            let pane_map = self.remote_panes.lock();
            let local_pane = match pane_map.get(&pane.pane_id) {
                Some(p) => {
                    let local_pane_id = p.lock().local_pane_id;
                    mux.get_pane(local_pane_id)
                }
                None => None,
            };

            if let Some(local_pane) = local_pane {
                let c = local_pane.get_cursor_position();
                // no capture, output case
                if c.x == 0 && c.y == 0 {
                    if let Some(text) = self.backlog.lock().remove(&pane.pane_id) {
                        if let Some(ref_pane) = pane_map.get(&pane.pane_id) {
                            let mut ref_pane = ref_pane.lock();
                            if let Err(err) = ref_pane.output_write.write_all(&text) {
                                log::error!("Failed to write tmux data to output: {:#}", err);
                            }
                        }
                    }
                } else {
                    // we have capture, so remove the backlog
                    let _ = self.backlog.lock().remove(&pane.pane_id);
                    if pane.cursor_x != 0 || pane.cursor_y != 0 {
                        self.set_pane_cursor_position(
                            &local_pane,
                            u64_to_usize_saturating(pane.cursor_x),
                            u64_to_usize_saturating(pane.cursor_y),
                        );
                    }
                }
                if pane.pane_active {
                    let gui_tabs = self.gui_tabs.lock();

                    let Some(local_tab) = gui_tabs.get(&pane.window_id) else {
                        anyhow::bail!("invalid tmux window id {}", pane.window_id);
                    };

                    match mux.get_tab(local_tab.tab_id) {
                        Some(tab) => {
                            tab.set_active_pane(&local_pane);
                            mux.notify(MuxNotification::PaneFocused(local_pane.pane_id()));
                        }
                        None => {}
                    }
                }
            }

            log::info!("new pane synced, id: {}", pane.pane_id);
        }

        Ok(())
    }

    fn sync_window_state(&self, windows: &[WindowItem], new_window: bool) -> anyhow::Result<()> {
        let Some(current_session) = *self.tmux_session.lock() else {
            return Ok(());
        };
        let mux = tmux_mux()?;

        if !new_window {
            let active_window_ids: HashSet<TmuxWindowId> = windows
                .iter()
                .filter(|window| window.session_id == current_session)
                .map(|window| window.window_id)
                .collect();
            let stale_window_ids: Vec<TmuxWindowId> = {
                let gui_tabs = self.gui_tabs.lock();
                gui_tabs
                    .keys()
                    .copied()
                    .filter(|window_id| !active_window_ids.contains(window_id))
                    .collect()
            };

            for stale_window_id in stale_window_ids {
                let _ = self.remove_detached_window(stale_window_id);
            }
        }

        self.create_gui_window();
        let mut gui_window = self.gui_window.lock();
        let gui_window_id = match gui_window.as_mut() {
            Some(x) => x,
            None => {
                anyhow::bail!("No tmux gui created");
            }
        };

        for window in windows.iter() {
            if window.session_id != current_session {
                continue;
            }

            let size = TerminalSize {
                rows: u64_to_usize_saturating(window.window_height),
                cols: u64_to_usize_saturating(window.window_width),
                pixel_width: 0,
                pixel_height: 0,
                dpi: 0,
            };

            let tab = Arc::new(Tab::new(&size));
            tab.set_title(&format!("{}", window.window_name));
            mux.add_tab_no_panes(&tab);

            let _ = self.add_attached_window(window, &tab.tab_id())?;

            let mut split_stack;
            let mut split_direction;

            let mut split_pane_index = 1;
            for l in &window.layout {
                match l {
                    WindowLayout::SinglePane(x) => {
                        let p = PaneItem {
                            session_id: window.session_id,
                            window_id: window.window_id,
                            _pane_index: 0,
                            cursor_x: 0,
                            cursor_y: 0,
                            pane_active: false,
                            pane_id: x.pane_id,
                            pane_width: x.pane_width,
                            pane_height: x.pane_height,
                            pane_left: x.pane_left,
                            pane_top: x.pane_top,
                        };
                        let local_pane = self.create_pane(&p).context("failed to create pane")?;
                        tab.assign_pane(&local_pane);
                        self.add_attached_pane(p.window_id, p.pane_id)?;
                        let _ = mux.add_pane(&local_pane);
                        break;
                    }

                    WindowLayout::SplitHorizontal(x) => {
                        split_direction = SplitDirection::Horizontal;
                        split_stack = x;
                    }

                    WindowLayout::SplitVertical(x) => {
                        split_direction = SplitDirection::Vertical;
                        split_stack = x;
                    }
                }

                for x in split_stack {
                    let p = PaneItem {
                        session_id: window.session_id,
                        window_id: window.window_id,
                        _pane_index: 0,
                        cursor_x: 0,
                        cursor_y: 0,
                        pane_active: false,
                        pane_id: x.pane_id,
                        pane_width: x.pane_width,
                        pane_height: x.pane_height,
                        pane_left: x.pane_left,
                        pane_top: x.pane_top,
                    };
                    let local_pane;
                    if !self.check_pane_attached(p.window_id, p.pane_id) {
                        local_pane = self.create_pane(&p).context("failed to create pane")?;
                        self.add_attached_pane(p.window_id, p.pane_id)?;
                        let _ = mux.add_pane(&local_pane);
                        if let None = tab.get_active_pane() {
                            tab.assign_pane(&local_pane);
                            split_pane_index = tab.get_active_idx();
                            continue;
                        }

                        split_pane_index = next_split_pane_index(tab.split_and_insert(
                            split_pane_index,
                            SplitRequest {
                                direction: split_direction,
                                target_is_second: false,
                                top_level: false,
                                size: SplitSize::Cells(
                                    if split_direction == SplitDirection::Horizontal {
                                        u64_to_usize_saturating(p.pane_width)
                                    } else {
                                        u64_to_usize_saturating(p.pane_height)
                                    },
                                ),
                            },
                            local_pane.clone(),
                        )?);
                    } else {
                        let pane_map = self.remote_panes.lock();
                        let local_pane_id = match pane_map.get(&p.pane_id) {
                            Some(x) => x.lock().local_pane_id,
                            None => anyhow::bail!("cannot find the local pane for {}", p.pane_id),
                        };

                        split_pane_index = match tab
                            .iter_panes_ignoring_zoom()
                            .iter()
                            .find(|x| x.pane.pane_id() == local_pane_id)
                        {
                            Some(x) => x.index,
                            None => {
                                log::info!("invalid pane id {local_pane_id}");
                                continue;
                            }
                        };
                        continue;
                    }
                }
            }

            mux.add_tab_to_window(&tab, **gui_window_id)?;
            gui_window_id.notify();

            let gui_tabs = self.gui_tabs.lock();
            let local_tab = match gui_tabs.get(&window.window_id) {
                Some(x) => x,
                None => {
                    log::info!(
                        "cannot find the local tab for tmux window {}",
                        window.window_id
                    );
                    continue;
                }
            };

            // For new window, we wait for nature ouput instead of capturing
            if !new_window {
                for p in local_tab.panes.iter() {
                    self.cmd_queue.lock().push_back(Box::new(CapturePane {
                        pane_id: *p,
                        history_limit: window.history_limit,
                    }));
                }
            }

            // To keep the active window last one to make it active after set the focus pane
            if !window.window_active {
                self.cmd_queue.lock().push_back(Box::new(ListAllPanes {
                    window_id: window.window_id,
                    prune: false,
                    layout_csum: window.layout_csum.clone(),
                }));
            }
        }

        // To keep the active window last one to make it active after set the focus pane
        match windows.iter().find(|w| w.window_active) {
            Some(window) => {
                self.cmd_queue.lock().push_back(Box::new(ListAllPanes {
                    window_id: window.window_id,
                    prune: false,
                    layout_csum: window.layout_csum.clone(),
                }));
            }
            None => {}
        }

        if *self.attach_state.lock() == AttachState::Init {
            self.cmd_queue.lock().push_back(Box::new(AttachDone));
        }

        TmuxDomainState::schedule_send_next_command(self.domain_id);

        Ok(())
    }

    pub fn subscribe_notification(&self) {
        let mut notification_sub_id = self.notification_sub_id.lock();
        if notification_sub_id.is_some() {
            return;
        }

        let Some(mux) = Mux::try_get() else {
            log::warn!("cannot subscribe tmux notifications without active mux");
            return;
        };
        let domain_id = self.domain_id;
        let sub_id = mux.subscribe(move |n| {
            // Domain lifetimes can outlive tmux sessions and a stale callback
            // would otherwise accumulate forever in mux subscribers.
            let Some(mux) = Mux::try_get() else {
                return false;
            };
            let Some(domain) = mux.get_domain(domain_id) else {
                return false;
            };
            if domain.downcast_ref::<TmuxDomain>().is_none() {
                return false;
            }

            if !promise::spawn::is_scheduler_configured() {
                return true;
            }

            promise::spawn::spawn_into_main_thread(async move {
                let Some(mux) = Mux::try_get() else {
                    return;
                };
                let Some(domain) = mux.get_domain(domain_id) else {
                    return;
                };
                let Some(tmux_domain) = domain.downcast_ref::<TmuxDomain>() else {
                    return;
                };

                if *tmux_domain.inner.attach_state.lock() == AttachState::Init {
                    return;
                }

                match n {
                    MuxNotification::PaneFocused(pane_id) => {
                        let tmux_pane_id = match tmux_domain
                            .inner
                            .remote_panes
                            .lock()
                            .iter()
                            .find(|(_, p)| p.lock().local_pane_id == pane_id)
                        {
                            Some((_, p)) => Some(p.lock().pane_id),
                            None => None,
                        };

                        if let Some(pane_id) = tmux_pane_id {
                            tmux_domain
                                .inner
                                .cmd_queue
                                .lock()
                                .push_back(Box::new(SelectPane { pane_id: pane_id }));
                            TmuxDomainState::schedule_send_next_command(domain_id);
                        }
                    }
                    MuxNotification::WindowInvalidated(window_id) => {
                        if let Some(window) = mux.get_window(window_id) {
                            let Some(tab) = window.get_active() else {
                                return;
                            };
                            let tmux_window_id = match tmux_domain
                                .inner
                                .gui_tabs
                                .lock()
                                .iter()
                                .find(|(_, t)| t.tab_id == tab.tab_id())
                            {
                                Some((_, t)) => Some(t.tmux_window_id),
                                None => None,
                            };
                            if let Some(window_id) = tmux_window_id {
                                tmux_domain.inner.cmd_queue.lock().push_back(Box::new(
                                    SelectWindow {
                                        window_id: window_id,
                                    },
                                ));
                                TmuxDomainState::schedule_send_next_command(domain_id);
                            }
                        }
                    }
                    _ => {}
                }
            })
            .detach();
            true
        });
        *notification_sub_id = Some(sub_id);
    }

    pub fn unsubscribe_notification(&self) {
        let Some(sub_id) = self.notification_sub_id.lock().take() else {
            return;
        };
        if let Some(mux) = Mux::try_get() {
            let _ = mux.unsubscribe(sub_id);
        }
    }
}

fn parse_sigil_number(text: &str) -> anyhow::Result<u64> {
    let Some(prefix) = text.chars().next() else {
        anyhow::bail!("missing tmux id sigil");
    };
    anyhow::ensure!(
        matches!(prefix, '$' | '%' | '@'),
        "unsupported tmux id sigil {prefix:?}"
    );

    let num = text
        .get(1..)
        .ok_or_else(|| anyhow!("wrong prefixed id"))?
        .parse()?;

    Ok(num)
}

fn parse_list_pane_item(line: &str) -> anyhow::Result<Option<PaneItem>> {
    if line.trim().is_empty() {
        return Ok(None);
    }

    let mut fields = line.split_whitespace();
    // These ids all have various sigils such as `$`, `%`, `@`,
    // so skip those prior to parsing them
    let session_id =
        parse_sigil_number(fields.next().ok_or_else(|| anyhow!("missing session_id"))?)?;
    let window_id = parse_sigil_number(fields.next().ok_or_else(|| anyhow!("missing window_id"))?)?;
    let pane_id = parse_sigil_number(fields.next().ok_or_else(|| anyhow!("missing pane_id"))?)?;
    let _pane_index = fields
        .next()
        .ok_or_else(|| anyhow!("missing pane_index"))?
        .parse()?;
    let cursor_x = fields
        .next()
        .ok_or_else(|| anyhow!("missing cursor_x"))?
        .parse()?;
    let cursor_y = fields
        .next()
        .ok_or_else(|| anyhow!("missing cursor_y"))?
        .parse()?;
    let pane_width = fields
        .next()
        .ok_or_else(|| anyhow!("missing pane_width"))?
        .parse()?;
    let pane_height = fields
        .next()
        .ok_or_else(|| anyhow!("missing pane_height"))?
        .parse()?;
    let pane_left = fields
        .next()
        .ok_or_else(|| anyhow!("missing pane_left"))?
        .parse()?;
    let pane_top = fields
        .next()
        .ok_or_else(|| anyhow!("missing pane_top"))?
        .parse()?;
    let pane_active = fields
        .next()
        .ok_or_else(|| anyhow!("missing pane_active"))?
        .parse::<usize>()?;

    Ok(Some(PaneItem {
        session_id,
        window_id,
        pane_id,
        _pane_index,
        cursor_x,
        cursor_y,
        pane_width,
        pane_height,
        pane_left,
        pane_top,
        pane_active: pane_active == 1,
    }))
}

const LIST_WINDOWS_FIELD_SEPARATOR: char = '\t';

fn parse_list_window_item(line: &str) -> anyhow::Result<Option<WindowItem>> {
    if line.trim().is_empty() {
        return Ok(None);
    }

    let mut fields = line.split(LIST_WINDOWS_FIELD_SEPARATOR);
    let session_id =
        parse_sigil_number(fields.next().ok_or_else(|| anyhow!("missing session_id"))?)?;
    let window_id = parse_sigil_number(fields.next().ok_or_else(|| anyhow!("missing window_id"))?)?;
    let window_width = fields
        .next()
        .ok_or_else(|| anyhow!("missing window_width"))?
        .parse()?;
    let window_height = fields
        .next()
        .ok_or_else(|| anyhow!("missing window_height"))?
        .parse()?;
    let window_active = fields
        .next()
        .ok_or_else(|| anyhow!("missing window_active"))?
        .parse::<usize>()?;
    let window_name = fields
        .next()
        .ok_or_else(|| anyhow!("missing window_name"))?
        .to_string();
    let window_layout = fields
        .next()
        .ok_or_else(|| anyhow!("missing window_layout"))?;
    let history_limit = fields
        .next()
        .ok_or_else(|| anyhow!("missing history_limit"))?
        .parse::<isize>()?;

    let (layout_csum, window_layout) = window_layout
        .split_once(',')
        .ok_or_else(|| anyhow!("missing window_layout body"))?;
    anyhow::ensure!(layout_csum.len() == 4, "invalid window_layout checksum");

    let layout = parse_layout(window_layout)?;

    Ok(Some(WindowItem {
        session_id,
        window_id,
        window_width,
        window_height,
        window_active: window_active == 1,
        window_name,
        layout,
        layout_csum: layout_csum.to_string(),
        history_limit,
    }))
}

fn normalize_capture_pane_output(unescaped: &str) -> String {
    let unescaped = unescaped
        .strip_suffix("\r\n")
        .or_else(|| unescaped.strip_suffix('\n'))
        .unwrap_or(unescaped);

    unescaped.replace("\r\n", "\n").replace('\n', "\r\n")
}

#[derive(Debug)]
pub(crate) struct ListAllPanes {
    pub window_id: TmuxWindowId,
    pub prune: bool,
    pub layout_csum: String,
}

impl TmuxCommand for ListAllPanes {
    fn get_command(&self, domain_id: DomainId) -> String {
        let Some(mux) = Mux::try_get() else {
            return "".to_string();
        };
        let domain = match mux.get_domain(domain_id) {
            Some(d) => d,
            None => return "".to_string(),
        };
        let tmux_domain = match domain.downcast_ref::<TmuxDomain>() {
            Some(t) => t,
            None => return "".to_string(),
        };

        let mut gui_tabs = tmux_domain.inner.gui_tabs.lock();

        let Some(local_tab) = gui_tabs.get_mut(&self.window_id) else {
            return "".to_string();
        };

        if local_tab.layout_csum.eq(&self.layout_csum) {
            if self.prune {
                return "".to_string();
            }
        } else {
            local_tab.layout_csum = self.layout_csum.clone();
        }

        format!(
            "list-panes -F '#{{session_id}} #{{window_id}} #{{pane_id}} \
            #{{pane_index}} #{{cursor_x}} #{{cursor_y}} #{{pane_width}} #{{pane_height}} \
            #{{pane_left}} #{{pane_top}} #{{pane_active}}' -t @{}\n",
            self.window_id
        )
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            let error = format!("list-pane in domain={domain_id} failed: {result:#?}");
            log::error!("{error}");
            anyhow::bail!("{error}");
        }
        let mut items = vec![];
        let mut pane_set = HashSet::new();
        for line in result.output.lines() {
            let Some(item) = parse_list_pane_item(line)? else {
                continue;
            };
            let pane_id = item.pane_id;
            pane_set.insert(pane_id);
            items.push(item);
        }

        log::debug!("panes in domain_id {}: {:?}", domain_id, items);
        let mux = tmux_mux()?;
        if let Some(domain) = mux.get_domain(domain_id) {
            if let Some(tmux_domain) = domain.downcast_ref::<TmuxDomain>() {
                if !self.prune {
                    return tmux_domain.inner.sync_pane_state(&items);
                } else {
                    return tmux_domain
                        .inner
                        .remove_detached_pane(self.window_id, &pane_set);
                }
            }
        }
        anyhow::bail!("Tmux domain lost");
    }
}

#[derive(Debug)]
pub(crate) struct ListAllWindows {
    pub session_id: TmuxSessionId,
    pub window_id: Option<TmuxWindowId>,
}

impl TmuxCommand for ListAllWindows {
    fn get_command(&self, _domain_id: DomainId) -> String {
        format!(
            concat!(
                "list-windows -F '",
                "#{{session_id}}\t",
                "#{{window_id}}\t",
                "#{{window_width}}\t",
                "#{{window_height}}\t",
                "#{{window_active}}\t",
                "#{{window_name}}\t",
                "#{{window_layout}}\t",
                "#{{history_limit}}' -t ${}\n",
            ),
            self.session_id
        )
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            let error = format!("list-window in domain={domain_id} failed: {result:#?}");
            log::error!("{error}");
            anyhow::bail!("{error}");
        }
        let mut items = vec![];

        for line in result.output.lines() {
            let Some(item) = parse_list_window_item(line)? else {
                continue;
            };

            if let Some(x) = self.window_id {
                if x != item.window_id {
                    continue;
                }
            }

            items.push(item);
        }

        log::debug!("layout in domain_id {}: {:#?}", domain_id, items);
        let mux = tmux_mux()?;
        if let Some(domain) = mux.get_domain(domain_id) {
            if let Some(tmux_domain) = domain.downcast_ref::<TmuxDomain>() {
                let new_window = if let Some(_x) = self.window_id {
                    true
                } else {
                    false
                };
                return tmux_domain.inner.sync_window_state(&items, new_window);
            }
        }
        anyhow::bail!("Tmux domain lost");
    }
}

#[derive(Debug)]
pub(crate) struct Resize {
    pub pane_id: TmuxPaneId,
    pub size: PtySize,
}

impl TmuxCommand for Resize {
    fn get_command(&self, domain_id: DomainId) -> String {
        let Some(mux) = Mux::try_get() else {
            return "".to_string();
        };
        let domain = match mux.get_domain(domain_id) {
            Some(d) => d,
            None => return "".to_string(),
        };
        let tmux_domain = match domain.downcast_ref::<TmuxDomain>() {
            Some(t) => t,
            None => return "".to_string(),
        };

        // Not in stable state for now, don't do resizing, otherwise it will cause tmux output
        // unexpected content.
        if *tmux_domain.inner.attach_state.lock() == AttachState::Init {
            return "".to_string();
        }

        let pane_map = tmux_domain.inner.remote_panes.lock();
        {
            let mut pane = match pane_map.get(&self.pane_id) {
                Some(x) => x.lock(),
                None => return "".to_string(),
            };

            if pane.pane_width == self.size.cols as u64 && pane.pane_height == self.size.rows as u64
            {
                return "".to_string();
            } else {
                pane.pane_width = self.size.cols as u64;
                pane.pane_height = self.size.rows as u64;
            }
        }

        let tmux_window_id = match pane_map.get(&self.pane_id) {
            Some(x) => x.lock().window_id,
            None => return "".to_string(),
        };

        let gui_tabs = tmux_domain.inner.gui_tabs.lock();
        let local_tab = match gui_tabs.get(&tmux_window_id) {
            Some(t) => t,
            None => return "".to_string(),
        };

        let size = match mux.get_tab(local_tab.tab_id) {
            Some(x) => x.get_size(),
            None => return "".to_string(),
        };

        let support_commands = tmux_domain.inner.support_commands.lock();

        if let Some(_x) = support_commands.get("resize-window") {
            format!(
                "resize-window -x {} -y {} -t @{}\nresize-pane -x {} -y {} -t %{}\n",
                size.cols, size.rows, tmux_window_id, self.size.cols, self.size.rows, self.pane_id
            )
        } else if let Some(x) = support_commands.get("refresh-client") {
            if x.contains("-C XxY") {
                format!(
                    "refresh-client -C {}x{}\nresize-pane -x {} -y {} -t %{}\n",
                    size.cols, size.rows, self.size.cols, self.size.rows, self.pane_id
                )
            } else {
                format!(
                    "refresh-client -C {},{}\nresize-pane -x {} -y {} -t %{}\n",
                    size.cols, size.rows, self.size.cols, self.size.rows, self.pane_id
                )
            }
        } else {
            log::info!("The tmux version is not supported");
            return "".to_string();
        }
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            let error = format!("resize-pane in domain={domain_id} failed: {result:#?}");
            log::error!("{error}");
            anyhow::bail!("{error}");
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct CapturePane {
    pane_id: TmuxPaneId,
    history_limit: isize,
}

impl TmuxCommand for CapturePane {
    fn get_command(&self, _domain_id: DomainId) -> String {
        format!(
            "capture-pane -p -t %{} -e -C -S {}\n",
            self.pane_id,
            self.history_limit.saturating_neg()
        )
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            let error = format!("capture-pane in domain={domain_id} failed: {result:#?}");
            log::error!("{error}");
            anyhow::bail!("{error}");
        }
        let mux = tmux_mux()?;
        let domain = match mux.get_domain(domain_id) {
            Some(d) => d,
            None => anyhow::bail!("Tmux domain lost"),
        };
        let tmux_domain = match domain.downcast_ref::<TmuxDomain>() {
            Some(t) => t,
            None => anyhow::bail!("Tmux domain lost"),
        };

        let unescaped = termwiz::tmux_cc::unvis(&result.output).context("unescape pane content")?;
        // capture-pane contents usually include a trailing newline from the guarded response.
        let unescaped = normalize_capture_pane_output(&unescaped);

        let pane_map = tmux_domain.inner.remote_panes.lock();
        if let Some(pane) = pane_map.get(&self.pane_id) {
            let mut pane = pane.lock();
            if let Some(p) = mux.get_pane(pane.local_pane_id) {
                tmux_domain.inner.set_pane_cursor_position(&p, 0, 0);
            }

            pane.output_write
                .write_all(unescaped.as_bytes())
                .context("writing capture pane result to output")?;
        }

        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct SendKeys {
    pub keys: Vec<u8>,
    pub pane: TmuxPaneId,
}
impl TmuxCommand for SendKeys {
    fn get_command(&self, _domain_id: DomainId) -> String {
        let mut s = String::new();
        for &byte in self.keys.iter() {
            write!(&mut s, "0x{:X} ", byte).expect("unable to write key");
        }
        format!("send-keys -t %{} {}\r", self.pane, s)
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            let error = format!("send-key in domain={domain_id} failed: {result:#?}");
            log::error!("{error}");
            anyhow::bail!("{error}");
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct ListCommands;
impl TmuxCommand for ListCommands {
    fn get_command(&self, _domain_id: DomainId) -> String {
        "list-commands\n".to_owned()
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            let error = format!("list-command in domain={domain_id} failed: {result:#?}");
            log::error!("{error}");
            anyhow::bail!("{error}");
        }
        let mux = tmux_mux()?;
        let domain = match mux.get_domain(domain_id) {
            Some(d) => d,
            None => anyhow::bail!("Tmux domain lost"),
        };
        let tmux_domain = match domain.downcast_ref::<TmuxDomain>() {
            Some(t) => t,
            None => anyhow::bail!("Tmux domain lost"),
        };

        let mut support_commands = tmux_domain.inner.support_commands.lock();

        for line in result.output.lines() {
            let Some(command_name) = line.split_whitespace().next() else {
                continue;
            };
            support_commands.insert(command_name.to_string(), line.to_string());
        }

        let mut cmd_queue = tmux_domain.inner.cmd_queue.as_ref().lock();
        if let Some(session) = *tmux_domain.inner.tmux_session.lock() {
            cmd_queue.push_back(Box::new(ListAllWindows {
                session_id: session,
                window_id: None,
            }));
            TmuxDomainState::schedule_send_next_command(domain_id);
        }

        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct SplitPane {
    pub pane_id: TmuxPaneId,
    pub direction: SplitDirection,
}

impl TmuxCommand for SplitPane {
    fn get_command(&self, _domain_id: DomainId) -> String {
        if self.direction == SplitDirection::Horizontal {
            format!("split-window -h -t %{}\n", self.pane_id)
        } else {
            format!("split-window -v -t %{}\n", self.pane_id)
        }
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            let error = format!("split-window in domain={domain_id} failed: {result:#?}");
            if let Some(mux) = Mux::try_get() {
                if let Some(domain) = mux.get_domain(domain_id) {
                    if let Some(tmux_domain) = domain.downcast_ref::<TmuxDomain>() {
                        let _ = tmux_domain
                            .inner
                            .fail_oldest_pending_split(anyhow!(error.clone()));
                    }
                }
            }
            log::error!("{error}");
            anyhow::bail!("{error}");
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct SelectWindow {
    pub window_id: TmuxWindowId,
}

impl TmuxCommand for SelectWindow {
    fn get_command(&self, _domain_id: DomainId) -> String {
        format!("select-window -t @{}\n", self.window_id)
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            let error = format!("select-window in domain={domain_id} failed: {result:#?}");
            log::error!("{error}");
            anyhow::bail!("{error}");
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct SelectPane {
    pub pane_id: TmuxPaneId,
}

impl TmuxCommand for SelectPane {
    fn get_command(&self, _domain_id: DomainId) -> String {
        format!("select-pane -t %{}\n", self.pane_id)
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            let error = format!("select-pane in domain={domain_id} failed: {result:#?}");
            log::error!("{error}");
            anyhow::bail!("{error}");
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct KillPane {
    pub pane_id: TmuxPaneId,
}

impl TmuxCommand for KillPane {
    fn get_command(&self, _domain_id: DomainId) -> String {
        format!("kill-pane -t %{}\n", self.pane_id)
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            let error = format!("kill-pane in domain={domain_id} failed: {result:#?}");
            log::error!("{error}");
            anyhow::bail!("{error}");
        }
        Ok(())
    }
}

// This is a dummy command which indicates the attaching is done, it prevents the tmux output
// the unexpected and unnecessary content when syncing with back end in attaching stage.
#[derive(Debug)]
pub(crate) struct AttachDone;
impl TmuxCommand for AttachDone {
    fn get_command(&self, _domain_id: DomainId) -> String {
        // The command doesn't matter, just give a legal simple command to let process_result called.
        "list-session\n".to_string()
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            let error = format!("list-session in domain={domain_id} failed: {result:#?}");
            log::error!("{error}");
            anyhow::bail!("{error}");
        }
        let mux = tmux_mux()?;
        let domain = match mux.get_domain(domain_id) {
            Some(d) => d,
            None => anyhow::bail!("Tmux domain lost"),
        };
        let tmux_domain = match domain.downcast_ref::<TmuxDomain>() {
            Some(t) => t,
            None => anyhow::bail!("Tmux domain lost"),
        };

        // Do nothing, just change the state.
        *tmux_domain.inner.attach_state.lock() = AttachState::Done;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Domain;
    use promise::spawn::ScopedExecutor;
    use std::sync::MutexGuard as StdMutexGuard;

    struct ScopedMux {
        prior: Option<Arc<Mux>>,
        _executor: ScopedExecutor,
        _guard: StdMutexGuard<'static, ()>,
    }

    impl ScopedMux {
        fn install(mux: Arc<Mux>) -> Self {
            let guard = crate::MUX_TEST_LOCK
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            let executor = ScopedExecutor::new();
            let prior = Mux::try_get();
            Mux::set_mux(&mux);
            Self {
                prior,
                _executor: executor,
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

    fn install_tmux_domain() -> (ScopedMux, Arc<TmuxDomain>) {
        let mux = Arc::new(Mux::new(None));
        let guard = ScopedMux::install(Arc::clone(&mux));

        let tmux_domain = Arc::new(TmuxDomain::new(0));
        let domain: Arc<dyn Domain> = tmux_domain.clone();
        mux.add_domain(&domain);

        (guard, tmux_domain)
    }

    #[test]
    fn remove_tmux_pane_state_entries_removes_requested_ids_only() {
        let mut remote_panes: HashMap<TmuxPaneId, Arc<Mutex<TmuxRemotePane>>> = HashMap::new();
        let removed_child_state = Arc::new(TmuxChildState::new());
        let retained_child_state = Arc::new(TmuxChildState::new());
        let (_read_removed, write_removed) = filedescriptor::socketpair().expect("socketpair");
        let (_read_retained, write_retained) = filedescriptor::socketpair().expect("socketpair");
        remote_panes.insert(
            11,
            Arc::new(Mutex::new(TmuxRemotePane {
                local_pane_id: 101,
                output_write: write_removed,
                child_state: removed_child_state.clone(),
                session_id: 1,
                window_id: 1,
                pane_id: 11,
                cursor_x: 0,
                cursor_y: 0,
                pane_width: 80,
                pane_height: 24,
                pane_left: 0,
                pane_top: 0,
            })),
        );
        remote_panes.insert(
            22,
            Arc::new(Mutex::new(TmuxRemotePane {
                local_pane_id: 202,
                output_write: write_retained,
                child_state: retained_child_state.clone(),
                session_id: 1,
                window_id: 1,
                pane_id: 22,
                cursor_x: 0,
                cursor_y: 0,
                pane_width: 80,
                pane_height: 24,
                pane_left: 0,
                pane_top: 0,
            })),
        );

        let mut backlog: HashMap<TmuxPaneId, Vec<u8>> = HashMap::new();
        backlog.insert(11, b"pane-11".to_vec());
        backlog.insert(22, b"pane-22".to_vec());
        backlog.insert(33, b"pane-33".to_vec());

        let removed_local_ids = TmuxDomainState::remove_tmux_pane_state_entries(
            &mut remote_panes,
            &mut backlog,
            &[11, 33],
        );

        assert_eq!(removed_local_ids, vec![101]);
        assert!(!remote_panes.contains_key(&11));
        assert!(remote_panes.contains_key(&22));
        assert!(!backlog.contains_key(&11));
        assert!(backlog.contains_key(&22));
        assert!(!backlog.contains_key(&33));
        assert_eq!(
            removed_child_state
                .try_wait()
                .map(|status| status.exit_code()),
            Some(0)
        );
        assert!(retained_child_state.try_wait().is_none());
    }

    #[test]
    fn parse_sigil_number_dollar_prefix() {
        assert_eq!(parse_sigil_number("$5").unwrap(), 5);
    }

    #[test]
    fn parse_sigil_number_percent_prefix() {
        assert_eq!(parse_sigil_number("%42").unwrap(), 42);
    }

    #[test]
    fn parse_sigil_number_at_prefix() {
        assert_eq!(parse_sigil_number("@100").unwrap(), 100);
    }

    #[test]
    fn parse_sigil_number_zero() {
        assert_eq!(parse_sigil_number("@0").unwrap(), 0);
    }

    #[test]
    fn parse_sigil_number_empty_after_sigil_is_error() {
        assert!(parse_sigil_number("$").is_err());
    }

    #[test]
    fn parse_sigil_number_no_chars_is_error() {
        assert!(parse_sigil_number("").is_err());
    }

    #[test]
    fn parse_sigil_number_non_numeric_is_error() {
        assert!(parse_sigil_number("$abc").is_err());
    }

    #[test]
    fn parse_sigil_number_rejects_unknown_prefix() {
        assert!(parse_sigil_number("x42").is_err());
    }

    #[test]
    fn parse_list_pane_item_accepts_repeated_spaces() {
        let item = parse_list_pane_item("  $7   @8   %9  2  10 20 80 24 0 1 1  ")
            .unwrap()
            .unwrap();

        assert_eq!(item.session_id, 7);
        assert_eq!(item.window_id, 8);
        assert_eq!(item.pane_id, 9);
        assert_eq!(item._pane_index, 2);
        assert_eq!(item.cursor_x, 10);
        assert_eq!(item.cursor_y, 20);
        assert_eq!(item.pane_width, 80);
        assert_eq!(item.pane_height, 24);
        assert_eq!(item.pane_left, 0);
        assert_eq!(item.pane_top, 1);
        assert!(item.pane_active);
    }

    #[test]
    fn parse_list_pane_item_skips_whitespace_only_lines() {
        assert!(parse_list_pane_item("   \t  ").unwrap().is_none());
    }

    #[test]
    fn parse_list_window_item_preserves_spaced_window_name() {
        let item =
            parse_list_window_item("$7\t@8\t120\t40\t1\tbuild logs\tabcd,158x40,0,0,72\t2000")
                .unwrap()
                .unwrap();

        assert_eq!(item.session_id, 7);
        assert_eq!(item.window_id, 8);
        assert_eq!(item.window_width, 120);
        assert_eq!(item.window_height, 40);
        assert!(item.window_active);
        assert_eq!(item.window_name, "build logs");
        assert_eq!(item.layout_csum, "abcd");
        assert_eq!(item.history_limit, 2000);
        assert_eq!(item.layout.len(), 1);
    }

    #[test]
    fn parse_list_window_item_rejects_layout_without_separator() {
        assert!(
            parse_list_window_item("$7\t@8\t120\t40\t1\tbuild logs\tabcd158x40,0,0,72\t2000")
                .is_err()
        );
    }

    #[test]
    fn send_keys_get_command_formats_hex_bytes() {
        let cmd = SendKeys {
            keys: vec![0x48, 0x69],
            pane: 7,
        };
        let output = cmd.get_command(0);
        assert!(output.starts_with("send-keys -t %7 "));
        assert!(output.contains("0x48"));
        assert!(output.contains("0x69"));
    }

    #[test]
    fn send_keys_get_command_empty_keys() {
        let cmd = SendKeys {
            keys: vec![],
            pane: 3,
        };
        let output = cmd.get_command(0);
        assert!(output.contains("send-keys -t %3"));
    }

    #[test]
    fn capture_pane_get_command_includes_pane_id_and_history() {
        let cmd = CapturePane {
            pane_id: 12,
            history_limit: 1000,
        };
        let output = cmd.get_command(0);
        assert!(output.contains("capture-pane"));
        assert!(output.contains("-t %12"));
        assert!(output.contains("-S -1000"));
    }

    #[test]
    fn capture_pane_get_command_saturates_min_history_limit() {
        let cmd = CapturePane {
            pane_id: 12,
            history_limit: isize::MIN,
        };
        let output = cmd.get_command(0);
        assert!(output.contains(&format!("-S {}", isize::MAX)));
    }

    #[test]
    fn list_commands_get_command() {
        let cmd = ListCommands;
        assert_eq!(cmd.get_command(0), "list-commands\n");
    }

    #[test]
    fn list_all_windows_get_command_uses_tab_delimiters() {
        let cmd = ListAllWindows {
            session_id: 9,
            window_id: None,
        };
        assert_eq!(
            cmd.get_command(0),
            "list-windows -F '#{session_id}\t#{window_id}\t#{window_width}\t#{window_height}\t#{window_active}\t#{window_name}\t#{window_layout}\t#{history_limit}' -t $9\n"
        );
    }

    #[test]
    fn normalize_capture_pane_output_strips_only_trailing_newline() {
        assert_eq!(
            normalize_capture_pane_output("alpha\nbeta\n"),
            "alpha\r\nbeta"
        );
    }

    #[test]
    fn normalize_capture_pane_output_preserves_existing_crlf() {
        assert_eq!(
            normalize_capture_pane_output("alpha\r\nbeta\r\n"),
            "alpha\r\nbeta"
        );
    }

    #[test]
    fn normalize_capture_pane_output_handles_unicode_without_newline() {
        assert_eq!(
            normalize_capture_pane_output("pane \u{03b1}"),
            "pane \u{03b1}"
        );
    }

    #[test]
    fn split_pane_horizontal_get_command() {
        let cmd = SplitPane {
            pane_id: 5,
            direction: SplitDirection::Horizontal,
        };
        assert_eq!(cmd.get_command(0), "split-window -h -t %5\n");
    }

    #[test]
    fn split_pane_vertical_get_command() {
        let cmd = SplitPane {
            pane_id: 9,
            direction: SplitDirection::Vertical,
        };
        assert_eq!(cmd.get_command(0), "split-window -v -t %9\n");
    }

    #[test]
    fn select_window_get_command() {
        let cmd = SelectWindow { window_id: 3 };
        assert_eq!(cmd.get_command(0), "select-window -t @3\n");
    }

    #[test]
    fn select_pane_get_command() {
        let cmd = SelectPane { pane_id: 17 };
        assert_eq!(cmd.get_command(0), "select-pane -t %17\n");
    }

    #[test]
    fn kill_pane_get_command() {
        let cmd = KillPane { pane_id: 23 };
        assert_eq!(cmd.get_command(0), "kill-pane -t %23\n");
    }

    #[test]
    fn attach_done_get_command() {
        let cmd = AttachDone;
        assert_eq!(cmd.get_command(0), "list-session\n");
    }

    #[test]
    fn session_changed_queues_list_commands_and_subscribes() {
        let (_mux_guard, tmux_domain) = install_tmux_domain();
        let domain_id = tmux_domain.domain_id();

        tmux_domain
            .inner
            .advance(Box::new(vec![Event::SessionChanged {
                session: 7,
                name: "main".to_string(),
            }]));

        assert_eq!(*tmux_domain.inner.tmux_session.lock(), Some(7));
        assert!(tmux_domain.inner.notification_sub_id.lock().is_some());

        let queue = tmux_domain.inner.cmd_queue.lock();
        assert_eq!(queue.len(), 1);
        assert_eq!(
            queue.front().unwrap().get_command(domain_id),
            "list-commands\n"
        );
    }

    #[test]
    fn list_commands_result_queues_window_listing_for_session() -> anyhow::Result<()> {
        let (_mux_guard, tmux_domain) = install_tmux_domain();
        let domain_id = tmux_domain.domain_id();
        *tmux_domain.inner.tmux_session.lock() = Some(9);

        let cmd = ListCommands;
        let result = Guarded {
            error: false,
            timestamp: 0,
            number: 0,
            flags: 0,
            output: "\n  list-windows   list-windows\n\n".to_string(),
        };

        cmd.process_result(domain_id, &result)?;

        let support_commands = tmux_domain.inner.support_commands.lock();
        assert!(support_commands.contains_key("list-windows"));
        assert!(!support_commands.contains_key(""));
        drop(support_commands);

        let queue = tmux_domain.inner.cmd_queue.lock();
        assert_eq!(queue.len(), 1);

        let queued = queue.front().unwrap().get_command(domain_id);
        assert!(queued.starts_with("list-windows -F "));
        assert!(queued.ends_with(" -t $9\n"));
        Ok(())
    }

    #[test]
    fn attach_done_process_result_marks_attach_state_done() -> anyhow::Result<()> {
        let (_mux_guard, tmux_domain) = install_tmux_domain();
        let domain_id = tmux_domain.domain_id();
        let cmd = AttachDone;
        let result = Guarded {
            error: false,
            timestamp: 0,
            number: 0,
            flags: 0,
            output: String::new(),
        };

        assert_eq!(*tmux_domain.inner.attach_state.lock(), AttachState::Init);
        cmd.process_result(domain_id, &result)?;
        assert_eq!(*tmux_domain.inner.attach_state.lock(), AttachState::Done);
        Ok(())
    }

    #[test]
    fn pane_item_debug_includes_fields() {
        let item = PaneItem {
            session_id: 1,
            window_id: 2,
            pane_id: 3,
            _pane_index: 0,
            cursor_x: 10,
            cursor_y: 20,
            pane_width: 80,
            pane_height: 24,
            pane_left: 0,
            pane_top: 0,
            pane_active: true,
        };
        let debug = format!("{:?}", item);
        assert!(debug.contains("session_id: 1"));
        assert!(debug.contains("pane_active: true"));
    }

    #[test]
    fn u64_to_usize_saturates_overflowing_cursor_values() {
        assert_eq!(u64_to_usize_saturating(42), 42);
        if usize::BITS < u64::BITS {
            assert_eq!(u64_to_usize_saturating(u64::MAX), usize::MAX);
        }
    }

    #[test]
    fn usize_to_u32_saturates_overflowing_cursor_values() {
        assert_eq!(usize_to_u32_saturating(42), 42);
        if usize::BITS > u32::BITS {
            assert_eq!(usize_to_u32_saturating(usize::MAX), u32::MAX);
        }
    }

    #[test]
    fn next_split_pane_index_saturates_at_usize_max() {
        assert_eq!(next_split_pane_index(0), 1);
        assert_eq!(next_split_pane_index(usize::MAX), usize::MAX);
    }
}
