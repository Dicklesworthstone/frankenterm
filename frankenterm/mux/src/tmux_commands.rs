use crate::domain::{DomainId, WriterWrapper};
use crate::localpane::LocalPane;
use crate::pane::{alloc_pane_id, PaneId};
use crate::tab::{SplitDirection, SplitRequest, SplitSize, Tab, TabId};
use crate::tmux::{
    AttachState, TmuxBacklogDrain, TmuxBacklogLimits, TmuxDomain, TmuxDomainState,
    TmuxEnqueueError, TmuxNotificationIntent,
    TmuxNotificationIntentRunDisposition, TmuxPaneOutputState, TmuxRemotePane, TmuxTab,
    TmuxTopologyBarrierEvent, NOTIFICATION_INTENT_DRAIN_QUANTUM,
};
use crate::tmux_pty::{TmuxChild, TmuxChildState, TmuxPty};
use crate::{
    Mux, MuxNotification, MuxNotificationEnvelope, MuxTopologyStamp, Pane, PaneOperationGuard,
    SplitCommitReceipt,
};
use anyhow::{anyhow, Context};
use frankenterm_term::TerminalSize;
use parking_lot::Mutex;
use portable_pty::{ExitStatus, MasterPty, PtySize};
use std::collections::HashSet;
use std::convert::TryFrom;
use std::fmt::{Debug, Write};
use std::io::Write as _;
use std::sync::{Arc, OnceLock};
use termwiz::tmux_cc::*;

/// Maximum payload retained in a single `SendKeys` command after queue merging.
const SEND_KEYS_MERGE_MAX_BYTES: usize = 16 * 1024;

pub(crate) trait TmuxCommand: Send + Debug {
    fn get_command(&self, domain_id: DomainId) -> String;
    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()>;

    /// Number of tmux guarded responses emitted by `get_command`.
    ///
    /// Most mailbox items issue one tmux command. Multi-command items must
    /// override this so a later response cannot be misattributed to the next
    /// mailbox item.
    fn expected_responses(&self) -> usize {
        1
    }

    fn mailbox_payload_bytes(&self) -> usize {
        0
    }

    fn try_merge_newer(&mut self, _newer: &dyn TmuxCommand) -> bool {
        false
    }

    fn as_send_keys(&self) -> Option<(TmuxPaneId, &[u8])> {
        None
    }

    fn as_resize(&self) -> Option<(TmuxPaneId, PtySize)> {
        None
    }

    fn as_select_window(&self) -> Option<TmuxWindowId> {
        None
    }

    fn as_select_pane(&self) -> Option<TmuxPaneId> {
        None
    }
}

fn tmux_mux() -> anyhow::Result<Arc<Mux>> {
    Mux::try_get().ok_or_else(|| anyhow!("tmux command requires active mux"))
}

fn u64_to_usize_saturating(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

#[cfg(test)]
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

struct TmuxNotificationIntentRunnableLease {
    owner: Arc<TmuxDomainState>,
    completed: bool,
}

impl Drop for TmuxNotificationIntentRunnableLease {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let _had_pending = self
            .owner
            .notification_intents
            .lock()
            .cancel_scheduled_runnable();
        if !self.owner.is_terminal() {
            log::error!(
                "tmux domain {} lost its notification-intent runnable; detaching rather than \
                 silently losing the authoritative final selection",
                self.owner.domain_id
            );
            self.owner.transition_to_exit_and_schedule_detach();
        }
    }
}

impl TmuxDomainState {
    fn fail_local_mirror_publication(&self, err: anyhow::Error) -> anyhow::Error {
        // A LocalPane drop normally asks its child signaller to kill the
        // corresponding remote pane.  Publication failures are local mirror
        // failures, not authority to mutate the live tmux session, so fence
        // every child before any partially built pane or tab can be dropped.
        // The absorbing terminal transition then removes all partial local
        // topology once the currently admitted operation returns.
        let remote_panes: Vec<_> = self.remote_panes.lock().values().cloned().collect();
        for remote_pane in remote_panes {
            remote_pane
                .lock()
                .child_state
                .mark_exited(ExitStatus::with_signal(
                    "tmux local mirror publication failed",
                ));
        }
        self.transition_to_exit_and_schedule_detach();
        err
    }

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
        if let Some(existing) = gui_tabs.get(&target.window_id) {
            anyhow::ensure!(
                existing.tab_id == *tab_id
                    && self
                        .mirror_index
                        .lock()
                        .remote_window_for_local_tab(*tab_id)
                        == Some(target.window_id),
                "tmux window {} was attached to conflicting local tab identities",
                target.window_id
            );
            return Ok(());
        }

        self.mirror_index
            .lock()
            .register_window(*tab_id, target.window_id)?;
        gui_tabs.insert(
            target.window_id,
            TmuxTab {
                tab_id: *tab_id,
                tmux_window_id: target.window_id,
                layout_csum: target.layout_csum.clone(),
                panes: HashSet::new(),
            },
        );
        Ok(())
    }

    fn retire_tmux_pane_state_entries(
        &self,
        pane_ids: &[TmuxPaneId],
    ) -> anyhow::Result<Vec<PaneId>> {
        let _retirement = self.pane_retirement.lock();
        let removed_panes = {
            let mut remote_panes = self.remote_panes.lock();
            let mut retired_panes = self.retired_panes.lock();
            let mut mirror_index = self.mirror_index.lock();
            let new_tombstones = pane_ids
                .iter()
                .filter(|pane_id| !retired_panes.contains(pane_id))
                .count();
            let Some(next_tombstone_count) = retired_panes.len().checked_add(new_tombstones) else {
                anyhow::bail!("tmux retired-pane tombstone accounting overflow");
            };
            anyhow::ensure!(
                next_tombstone_count <= super::tmux::RETIRED_PANE_TOMBSTONE_LIMIT,
                "tmux retired-pane tombstone cap {} exceeded",
                super::tmux::RETIRED_PANE_TOMBSTONE_LIMIT
            );

            let mut removed = Vec::with_capacity(pane_ids.len());
            for pane_id in pane_ids {
                retired_panes.insert(*pane_id);
                let remote = remote_panes.remove(pane_id);
                let indexed_local = mirror_index.unregister_pane(*pane_id)?;
                match (remote, indexed_local) {
                    (Some(remote), Some(local_pane_id)) => {
                        removed.push((*pane_id, local_pane_id, remote));
                    }
                    (None, None) => {}
                    (Some(_), None) => {
                        anyhow::bail!(
                            "tmux pane {pane_id} existed without a reverse-index identity"
                        );
                    }
                    (None, Some(local_pane_id)) => {
                        anyhow::bail!(
                            "tmux pane {pane_id} reverse index pointed at missing local pane \
                             {local_pane_id}"
                        );
                    }
                }
            }
            removed
        };

        let mut local_pane_ids = Vec::with_capacity(removed_panes.len());
        for (pane_id, indexed_local_pane_id, remote) in removed_panes {
            // Tombstone publication is the admission cutoff. A producer that
            // passed its per-pane tombstone check first linearizes before
            // retirement; this lock waits for it, then publishes Retired.
            let mut remote = remote.lock();
            anyhow::ensure!(
                remote.local_pane_id == indexed_local_pane_id,
                "tmux pane {pane_id} reverse index named local pane {} but the mirror named {}",
                indexed_local_pane_id,
                remote.local_pane_id
            );
            remote.output_state = TmuxPaneOutputState::Retired;
            remote
                .child_state
                .mark_exited(ExitStatus::with_exit_code(0));
            local_pane_ids.push(remote.local_pane_id);
        }

        // Every producer admitted before the tombstone has completed, and all
        // later output is rejected, before backlog cleanup.
        self.backlog.lock().remove_many(pane_ids);
        Ok(local_pane_ids)
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
                let removed = gui_tabs
                    .remove(&window_id)
                    .context("empty tmux tab disappeared during retirement")?;
                let indexed_tab = self.mirror_index.lock().unregister_window(window_id)?;
                anyhow::ensure!(
                    indexed_tab == Some(removed.tab_id),
                    "tmux window {window_id} reverse index did not name retired local tab {}",
                    removed.tab_id
                );
            }

            (tab_id, to_remove, tab_empty)
        };

        let local_pane_ids = self.retire_tmux_pane_state_entries(&to_remove)?;

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
                Some(tab) => {
                    let indexed_tab = self.mirror_index.lock().unregister_window(window_id)?;
                    anyhow::ensure!(
                        indexed_tab == Some(tab.tab_id),
                        "tmux window {window_id} reverse index did not name retired local tab {}",
                        tab.tab_id
                    );
                    tab
                }
                None => {
                    anyhow::ensure!(
                        self.mirror_index
                            .lock()
                            .unregister_window(window_id)?
                            .is_none(),
                        "tmux window {window_id} had a reverse index without an attached tab"
                    );
                    return Ok(());
                }
            }
        };

        let detached_panes: Vec<_> = tab.panes.iter().copied().collect();
        let local_pane_ids = self.retire_tmux_pane_state_entries(&detached_panes)?;

        let mux = tmux_mux()?;
        for pane_id in local_pane_ids {
            mux.remove_pane(pane_id);
        }
        mux.remove_tab(tab.tab_id);

        Ok(())
    }

    fn prepare_pane_capture(&self, pane_id: TmuxPaneId) -> anyhow::Result<bool> {
        let pane_map = self.remote_panes.lock();
        let pane = pane_map
            .get(&pane_id)
            .with_context(|| format!("cannot prepare capture for missing tmux pane {pane_id}"))?;
        let mut pane = pane.lock();
        match pane.output_state {
            TmuxPaneOutputState::Fresh => {
                pane.output_state = TmuxPaneOutputState::AwaitingCapture;
                Ok(true)
            }
            TmuxPaneOutputState::AwaitingCapture | TmuxPaneOutputState::Captured => Ok(false),
            TmuxPaneOutputState::Ready => Ok(false),
            TmuxPaneOutputState::Retired => {
                anyhow::bail!("cannot prepare capture for retired tmux pane {pane_id}")
            }
        }
    }

    fn create_pane(&self, pane: &PaneItem) -> anyhow::Result<Arc<dyn Pane>> {
        let local_pane_id = alloc_pane_id()?;
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
            output_state: TmuxPaneOutputState::Fresh,
        }));

        let pane_pty = TmuxPty {
            domain_id: self.domain_id,
            reader: output_read,
            cmd_queue: Arc::clone(&self.cmd_queue),
            master_pane: Arc::clone(&ref_pane),
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
            Arc::clone(&child_state),
            self.registered_owner_weak()?,
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

        let local_pane: Arc<dyn Pane> = Arc::new(LocalPane::new(
            local_pane_id,
            terminal,
            Box::new(child),
            Box::new(pane_pty),
            Box::new(writer),
            self.domain_id,
            command_description,
        ));

        let mut pane_map = self.remote_panes.lock();
        if self.retired_panes.lock().contains(&pane.pane_id) {
            child_state.mark_exited(ExitStatus::with_signal(
                "retired tmux remote pane identity reuse",
            ));
            anyhow::bail!(
                "tmux attempted to reuse retired remote pane id {}",
                pane.pane_id
            );
        }
        if pane_map.contains_key(&pane.pane_id) {
            child_state.mark_exited(ExitStatus::with_signal(
                "duplicate tmux remote pane identity",
            ));
            anyhow::bail!(
                "tmux remote pane {} is already mapped to a local pane",
                pane.pane_id
            );
        }
        if let Err(err) = self
            .mirror_index
            .lock()
            .register_pane(local_pane_id, pane.pane_id)
        {
            child_state.mark_exited(ExitStatus::with_signal(
                "conflicting tmux pane reverse-index identity",
            ));
            return Err(err.context("failed to register tmux pane reverse index"));
        }
        pane_map.insert(pane.pane_id, ref_pane);
        drop(pane_map);

        Ok(local_pane)
    }

    fn finish_fresh_split(&self, pane_id: TmuxPaneId) -> anyhow::Result<()> {
        let remote_pane = {
            let pane_map = self.remote_panes.lock();
            pane_map
                .get(&pane_id)
                .cloned()
                .with_context(|| format!("cannot finish missing split tmux pane {pane_id}"))?
        };
        let mut remote_pane = remote_pane.lock();
        anyhow::ensure!(
            remote_pane.output_state == TmuxPaneOutputState::Fresh,
            "split tmux pane {pane_id} reached {:?} before initial stream commit",
            remote_pane.output_state
        );

        let limits = TmuxBacklogLimits::current();
        let backlog_drain = {
            let mut backlog = self.backlog.lock();
            backlog.refresh_limits(limits);
            anyhow::ensure!(
                !backlog.requires_global_resync(),
                "tmux split pane {pane_id} cannot recover from a global output gap"
            );
            backlog.take(pane_id)
        };
        match backlog_drain {
            Some(TmuxBacklogDrain::ResyncRequired) => {
                anyhow::bail!("tmux split pane {pane_id} initial output is gapped")
            }
            Some(TmuxBacklogDrain::Bytes(bytes)) => {
                let (first, second) = bytes.as_slices();
                remote_pane
                    .output_write
                    .write_all(first)
                    .and_then(|()| remote_pane.output_write.write_all(second))
                    .context("writing complete pre-publication split-pane stream")?;
            }
            None => {}
        }
        remote_pane.output_state = TmuxPaneOutputState::Ready;
        Ok(())
    }

    pub fn split_pane(
        &self,
        mux: &Arc<Mux>,
        target: &PaneOperationGuard,
        remote_id: TmuxPaneId,
        split_request: SplitRequest,
    ) -> anyhow::Result<SplitCommitReceipt> {
        anyhow::ensure!(
            target.belongs_to(mux),
            "tmux split target belongs to another mux registration"
        );
        let (_domain_id, local_window_id, tab) = target.exact_location()?;

        let pane_index = match tab
            .iter_panes_ignoring_zoom()
            .iter()
            .find(|positioned| Arc::ptr_eq(&positioned.pane, target.pane()))
        {
            Some(p) => p.index,
            None => anyhow::bail!(
                "exact tmux split target registration {} is not tiled",
                target.pane_id()
            ),
        };

        let split_size = match tab.compute_split_size(pane_index, split_request) {
            Some(s) => s,
            None => anyhow::bail!("invalid pane index {}", pane_index),
        };

        let remote_window_id = self
            .mirror_index
            .lock()
            .remote_window_for_local_tab(tab.tab_id())
            .with_context(|| format!("No tmux mirror for exact tab {}", tab.tab_id()))?;

        let p = PaneItem {
            session_id: 0,
            window_id: remote_window_id,
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

        let pane = self.create_pane(&p).map_err(|err| {
            self.fail_local_mirror_publication(err.context("failed to create pane"))
        })?;
        if let Some(config) = target.with_pane(|pane| pane.get_config()) {
            pane.set_config(config);
        }
        tab.split_and_insert(pane_index, split_request, Arc::clone(&pane))
            .map_err(|err| {
                self.fail_local_mirror_publication(
                    err.context("failed to insert tmux pane into local tab"),
                )
            })?;

        self.add_attached_pane(remote_window_id, remote_id)
            .map_err(|err| {
                self.fail_local_mirror_publication(
                    err.context("failed to attach tmux pane to local window state"),
                )
            })?;

        mux.add_pane(&pane).map_err(|err| {
            self.fail_local_mirror_publication(
                err.context("failed to publish tmux pane in local mux"),
            )
        })?;
        let registration = mux.capture_pane_registration(&pane).ok_or_else(|| {
            self.fail_local_mirror_publication(anyhow!(
                "published tmux pane {} has no exact mux registration",
                pane.pane_id()
            ))
        })?;
        self.finish_fresh_split(remote_id).map_err(|err| {
            self.fail_local_mirror_publication(
                err.context("failed to commit split tmux pane initial output"),
            )
        })?;

        Ok(SplitCommitReceipt::from_exact_parts(
            pane,
            registration,
            tab,
            local_window_id,
            split_size.second,
        ))
    }

    fn sync_pane_state(&self, panes: &[PaneItem]) -> anyhow::Result<()> {
        let Some(current_session) = *self.tmux_session.lock() else {
            return Ok(());
        };
        let mux = tmux_mux()?;
        if self.backlog.lock().requires_global_resync() {
            anyhow::bail!(
                "tmux output backlog lost pane identity; refusing partial per-window recovery"
            );
        }

        for pane in panes.iter() {
            if pane.session_id != current_session
                || !self.check_pane_attached(pane.window_id, pane.pane_id)
            {
                continue;
            }

            let remote_pane = {
                let pane_map = self.remote_panes.lock();
                pane_map.get(&pane.pane_id).cloned().with_context(|| {
                    format!(
                        "tmux pane {} is attached but has no local remote-pane gate",
                        pane.pane_id
                    )
                })?
            };
            let mut remote_pane = remote_pane.lock();
            let local_pane = mux.get_pane(remote_pane.local_pane_id).with_context(|| {
                format!(
                    "tmux pane {} maps to missing local pane {}",
                    pane.pane_id, remote_pane.local_pane_id
                )
            })?;

            let backlog_drain = self.backlog.lock().take(pane.pane_id);
            let mut apply_snapshot_cursor = true;
            match (remote_pane.output_state, backlog_drain) {
                (_, Some(TmuxBacklogDrain::ResyncRequired)) => {
                    anyhow::bail!(
                        "tmux pane {} output backlog is gapped; refusing unsafe textual replay",
                        pane.pane_id
                    );
                }
                (TmuxPaneOutputState::Fresh, Some(TmuxBacklogDrain::Bytes(text))) => {
                    apply_snapshot_cursor = text.is_empty();
                    let (first, second) = text.as_slices();
                    remote_pane
                        .output_write
                        .write_all(first)
                        .and_then(|()| remote_pane.output_write.write_all(second))
                        .context("writing complete pre-attach tmux stream to local pane")?;
                }
                (TmuxPaneOutputState::Fresh, None) | (TmuxPaneOutputState::Captured, None) => {}
                (TmuxPaneOutputState::Captured, Some(TmuxBacklogDrain::Bytes(text)))
                    if !text.is_empty() =>
                {
                    anyhow::bail!(
                        "tmux pane {} produced {} bytes while capture publication was pending; \
                         refusing cursor-ambiguous textual replay",
                        pane.pane_id,
                        text.len()
                    );
                }
                (TmuxPaneOutputState::Captured, Some(TmuxBacklogDrain::Bytes(_))) => {}
                (TmuxPaneOutputState::AwaitingCapture, _) => {
                    anyhow::bail!(
                        "tmux pane {} list state overtook its required capture callback",
                        pane.pane_id
                    );
                }
                (TmuxPaneOutputState::Ready, Some(_)) => {
                    anyhow::bail!(
                        "ready tmux pane {} retained an impossible preparation backlog",
                        pane.pane_id
                    );
                }
                (TmuxPaneOutputState::Ready, None) => {}
                (TmuxPaneOutputState::Retired, _) => {
                    anyhow::bail!(
                        "tmux pane {} was retired during state synchronization",
                        pane.pane_id
                    );
                }
            }

            if remote_pane.output_state != TmuxPaneOutputState::Ready {
                if apply_snapshot_cursor {
                    let row = pane.cursor_y.saturating_add(1);
                    let col = pane.cursor_x.saturating_add(1);
                    write!(&mut remote_pane.output_write, "\u{1b}[{row};{col}H")
                        .context("serializing tmux cursor after initial pane stream")?;
                }
                remote_pane.output_state = TmuxPaneOutputState::Ready;
            }
            remote_pane.session_id = pane.session_id;
            remote_pane.window_id = pane.window_id;
            remote_pane.cursor_x = pane.cursor_x;
            remote_pane.cursor_y = pane.cursor_y;
            remote_pane.pane_width = pane.pane_width;
            remote_pane.pane_height = pane.pane_height;
            remote_pane.pane_left = pane.pane_left;
            remote_pane.pane_top = pane.pane_top;
            drop(remote_pane);

            if pane.pane_active {
                let gui_tabs = self.gui_tabs.lock();

                let Some(local_tab) = gui_tabs.get(&pane.window_id) else {
                    anyhow::bail!("invalid tmux window id {}", pane.window_id);
                };

                if let Some(tab) = mux.get_tab(local_tab.tab_id) {
                    let _ = tab.set_active_pane_for_mux(&local_pane, &mux);
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
        let mut required_commands: Vec<Box<dyn TmuxCommand>> = Vec::new();

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
                self.remove_detached_window(stale_window_id)?;
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
            mux.add_tab_no_panes(&tab).map_err(|err| {
                self.fail_local_mirror_publication(
                    err.context("failed to register tmux tab in local mux state"),
                )
            })?;

            self.add_attached_window(window, &tab.tab_id())
                .map_err(|err| {
                    self.fail_local_mirror_publication(
                        err.context("failed to attach tmux window to local tab state"),
                    )
                })?;

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
                        let local_pane = self.create_pane(&p).map_err(|err| {
                            self.fail_local_mirror_publication(
                                err.context("failed to create tmux pane"),
                            )
                        })?;
                        tab.assign_pane(&local_pane);
                        self.add_attached_pane(p.window_id, p.pane_id)
                            .map_err(|err| {
                                self.fail_local_mirror_publication(
                                    err.context("failed to attach tmux pane to local window state"),
                                )
                            })?;
                        mux.add_pane(&local_pane).map_err(|err| {
                            self.fail_local_mirror_publication(
                                err.context("failed to publish tmux pane in local mux"),
                            )
                        })?;
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
                        local_pane = self.create_pane(&p).map_err(|err| {
                            self.fail_local_mirror_publication(
                                err.context("failed to create tmux pane"),
                            )
                        })?;
                        self.add_attached_pane(p.window_id, p.pane_id)
                            .map_err(|err| {
                                self.fail_local_mirror_publication(
                                    err.context("failed to attach tmux pane to local window state"),
                                )
                            })?;
                        mux.add_pane(&local_pane).map_err(|err| {
                            self.fail_local_mirror_publication(
                                err.context("failed to publish tmux pane in local mux"),
                            )
                        })?;
                        if let None = tab.get_active_pane() {
                            tab.assign_pane(&local_pane);
                            split_pane_index = tab.get_active_idx();
                            continue;
                        }

                        split_pane_index = next_split_pane_index(
                            tab.split_and_insert(
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
                            )
                            .map_err(|err| {
                                self.fail_local_mirror_publication(
                                    err.context("failed to insert tmux pane into local tab"),
                                )
                            })?,
                        );
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

            mux.add_tab_to_window(&tab, **gui_window_id)
                .map_err(|err| {
                    self.fail_local_mirror_publication(
                        err.context("failed to publish tmux tab in local mux window"),
                    )
                })?;
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
                    if self.prepare_pane_capture(*p)? {
                        required_commands.push(Box::new(CapturePane {
                            pane_id: *p,
                            history_limit: window.history_limit,
                        }));
                    }
                }
            }

            // To keep the active window last one to make it active after set the focus pane
            if !window.window_active {
                required_commands.push(Box::new(ListAllPanes {
                    window_id: window.window_id,
                    prune: false,
                    layout_csum: window.layout_csum.clone(),
                }));
            }
        }

        // To keep the active window last one to make it active after set the focus pane
        match windows.iter().find(|w| w.window_active) {
            Some(window) => {
                required_commands.push(Box::new(ListAllPanes {
                    window_id: window.window_id,
                    prune: false,
                    layout_csum: window.layout_csum.clone(),
                }));
            }
            None => {}
        }

        if *self.attach_state.lock() == AttachState::Init {
            required_commands.push(Box::new(AttachDone));
        }

        self.enqueue_required_batch(required_commands, "window-state synchronization")
    }

    fn notification_targets_domain(&self, intent: TmuxNotificationIntent) -> bool {
        if *self.attach_state.lock() == AttachState::Init {
            return false;
        }
        match intent {
            TmuxNotificationIntent::PaneFocused(local_pane_id) => self
                .mirror_index
                .lock()
                .remote_pane_for_local(local_pane_id)
                .is_some(),
            TmuxNotificationIntent::WindowInvalidated(local_window_id) => self
                .gui_window
                .lock()
                .as_ref()
                .is_some_and(|window| window.window_id == local_window_id),
        }
    }

    fn ingest_mux_notification(
        self: &Arc<Self>,
        envelope: MuxNotificationEnvelope,
    ) -> bool {
        self.notification_intent_telemetry.record_received();
        let MuxNotificationEnvelope {
            notification,
            topology,
        } = envelope;
        let is_topology = notification.is_topology();
        let event = match notification {
            MuxNotification::PaneFocused(pane_id) => {
                let intent = TmuxNotificationIntent::PaneFocused(pane_id);
                if self.notification_targets_domain(intent) {
                    TmuxTopologyBarrierEvent::Intent(intent)
                } else {
                    TmuxTopologyBarrierEvent::Barrier
                }
            }
            MuxNotification::WindowInvalidated(window_id) => {
                let intent = TmuxNotificationIntent::WindowInvalidated(window_id);
                if self.notification_targets_domain(intent) {
                    TmuxTopologyBarrierEvent::Intent(intent)
                } else {
                    TmuxTopologyBarrierEvent::Barrier
                }
            }
            _ => TmuxTopologyBarrierEvent::Barrier,
        };

        if self.is_terminal() {
            return false;
        }
        match topology {
            MuxTopologyStamp::NonTopology => {
                if is_topology {
                    log::error!(
                        "tmux domain {} received a topology notification without a revision; \
                         detaching instead of guessing event order",
                        self.domain_id
                    );
                    self.transition_to_exit_and_schedule_detach();
                    return false;
                }
                self.notification_intent_telemetry.record_prefiltered();
            }
            MuxTopologyStamp::Exhausted => {
                log::error!(
                    "tmux domain {} observed exhausted mux topology authority; detaching instead \
                     of accepting unordered selection work",
                    self.domain_id
                );
                self.transition_to_exit_and_schedule_detach();
                return false;
            }
            MuxTopologyStamp::Revision(revision) => {
                if !is_topology {
                    log::error!(
                        "tmux domain {} received a non-topology notification carrying revision \
                         {}; detaching",
                        self.domain_id,
                        revision.get()
                    );
                    self.transition_to_exit_and_schedule_detach();
                    return false;
                }
                let prefiltered = event == TmuxTopologyBarrierEvent::Barrier;
                let observation = match self
                    .notification_intents
                    .lock()
                    .observe_topology_event(revision, event)
                {
                    Ok(observation) => observation,
                    Err(err) => {
                        metrics::counter!(
                            "mux.tmux.notification_intent.ordering_failures"
                        )
                        .increment(1);
                        log::error!(
                            "tmux domain {} failed to order mux topology revision {}: {err:#}; \
                             detaching",
                            self.domain_id,
                            revision.get()
                        );
                        self.transition_to_exit_and_schedule_detach();
                        return false;
                    }
                };
                if prefiltered || observation.stale {
                    self.notification_intent_telemetry.record_prefiltered();
                }
                self.notification_intent_telemetry
                    .record_coalesced(observation.coalesced);
                if observation.closed {
                    return false;
                }
                if observation.schedule {
                    if !promise::spawn::is_scheduler_configured() {
                        log::error!(
                            "tmux domain {} cannot schedule authoritative selection intent at \
                             topology revision {} because the main-thread scheduler is \
                             unavailable; detaching",
                            self.domain_id,
                            revision.get()
                        );
                        self.transition_to_exit_and_schedule_detach();
                        return false;
                    }
                    self.spawn_claimed_notification_intent_runnable();
                }
            }
        }
        true
    }

    fn spawn_claimed_notification_intent_runnable(self: &Arc<Self>) {
        self.notification_intent_telemetry.record_scheduled();
        let owner = Arc::clone(self);
        let lease = TmuxNotificationIntentRunnableLease {
            owner: Arc::clone(self),
            completed: false,
        };
        promise::spawn::spawn_into_main_thread(async move {
            let mut lease = lease;
            let disposition = owner.run_notification_intent_runnable();
            lease.completed = true;
            if disposition == TmuxNotificationIntentRunDisposition::Reschedule {
                owner.spawn_claimed_notification_intent_runnable();
            }
        })
        .detach();
    }

    fn run_notification_intent_runnable(
        self: &Arc<Self>,
    ) -> TmuxNotificationIntentRunDisposition {
        let Some(mux) = Mux::try_get() else {
            self.transition_to_exit_and_schedule_detach();
            return TmuxNotificationIntentRunDisposition::Closed;
        };
        let Some(domain) = mux.get_domain(self.domain_id) else {
            self.transition_to_exit_and_schedule_detach();
            return TmuxNotificationIntentRunDisposition::Closed;
        };
        let Some(tmux_domain) = domain.downcast_ref::<TmuxDomain>() else {
            self.transition_to_exit_and_schedule_detach();
            return TmuxNotificationIntentRunDisposition::Closed;
        };
        if !Arc::ptr_eq(self, &tmux_domain.inner) {
            self.transition_to_exit_and_schedule_detach();
            return TmuxNotificationIntentRunDisposition::Closed;
        }

        self.with_active_lifecycle(|| self.drain_notification_intents(&mux))
            .unwrap_or_else(|| {
                self.notification_intents.lock().close();
                TmuxNotificationIntentRunDisposition::Closed
            })
    }

    fn resolve_notification_intent_command(
        &self,
        mux: &Arc<Mux>,
        intent: TmuxNotificationIntent,
    ) -> Option<Box<dyn TmuxCommand>> {
        match intent {
            TmuxNotificationIntent::PaneFocused(local_pane_id) => {
                let remote_pane_id = self
                    .mirror_index
                    .lock()
                    .remote_pane_for_local(local_pane_id)?;
                Some(Box::new(SelectPane {
                    pane_id: remote_pane_id,
                }))
            }
            TmuxNotificationIntent::WindowInvalidated(local_window_id) => {
                let owns_window = self
                    .gui_window
                    .lock()
                    .as_ref()
                    .is_some_and(|window| window.window_id == local_window_id);
                if !owns_window {
                    return None;
                }
                let window = mux.get_window(local_window_id)?;
                let active_tab = window.get_active()?;
                let remote_window_id = self
                    .mirror_index
                    .lock()
                    .remote_window_for_local_tab(active_tab.tab_id())?;
                Some(Box::new(SelectWindow {
                    window_id: remote_window_id,
                }))
            }
        }
    }

    fn drain_notification_intents(
        &self,
        mux: &Arc<Mux>,
    ) -> TmuxNotificationIntentRunDisposition {
        let mut consumed = 0usize;
        while consumed < NOTIFICATION_INTENT_DRAIN_QUANTUM {
            let batch = self.notification_intents.lock().take_ordered_batch();
            if batch.iter().all(Option::is_none) {
                return self.notification_intents.lock().finish_quantum();
            }

            for index in 0..batch.len() {
                let Some(sequenced) = batch[index] else {
                    continue;
                };
                consumed = consumed.saturating_add(1);
                if !self.notification_intents.lock().is_current(sequenced) {
                    self.notification_intent_telemetry.record_coalesced(1);
                    continue;
                }

                let Some(command) =
                    self.resolve_notification_intent_command(mux, sequenced.intent)
                else {
                    self.notification_intent_telemetry.record_dropped_stale();
                    continue;
                };

                // Resolution may have crossed a newer callback. Do not admit
                // an already superseded selection ahead of its final intent.
                if !self.notification_intents.lock().is_current(sequenced) {
                    self.notification_intent_telemetry.record_coalesced(1);
                    continue;
                }

                let remaining = if index == 0 { batch[1] } else { None };
                let (enqueue_result, superseded) = {
                    let mut cmd_queue = self.cmd_queue.lock();
                    let enqueue_result = cmd_queue.push_back(command);
                    let superseded = if enqueue_result == Err(TmuxEnqueueError::Full) {
                        self.notification_intents
                            .lock()
                            .wait_for_capacity(sequenced, remaining)
                    } else {
                        0
                    };
                    (enqueue_result, superseded)
                };
                self.notification_intent_telemetry
                    .record_coalesced(superseded);

                match enqueue_result {
                    Ok(()) => {
                        self.notification_intent_telemetry.record_applied();
                        TmuxDomainState::schedule_send_next_command(self.domain_id);
                    }
                    Err(TmuxEnqueueError::Full) => {
                        self.notification_intent_telemetry.record_backpressured();
                        return self.notification_intents.lock().finish_quantum();
                    }
                    Err(TmuxEnqueueError::Closed) => {
                        self.notification_intents.lock().close();
                        return TmuxNotificationIntentRunDisposition::Closed;
                    }
                }
            }
        }

        self.notification_intents.lock().finish_quantum()
    }

    pub(crate) fn wake_notification_intent_capacity(domain_id: DomainId) {
        let Some(mux) = Mux::try_get() else {
            return;
        };
        let Some(domain) = mux.get_domain(domain_id) else {
            return;
        };
        let Some(tmux_domain) = domain.downcast_ref::<TmuxDomain>() else {
            return;
        };
        let owner = Arc::clone(&tmux_domain.inner);
        if !owner.notification_intents.lock().capacity_available() {
            return;
        }
        if !promise::spawn::is_scheduler_configured() {
            log::error!(
                "tmux domain {domain_id} regained command capacity without a main-thread \
                 scheduler; detaching instead of stranding its final selection intent"
            );
            owner.transition_to_exit_and_schedule_detach();
            return;
        }
        owner.spawn_claimed_notification_intent_runnable();
    }

    pub fn subscribe_notification(&self) -> anyhow::Result<()> {
        let _subscription_gate = self.notification_subscription_gate.lock();
        if self.notification_sub_id.lock().is_some() {
            return Ok(());
        }

        let mux =
            Mux::try_get().context("cannot subscribe tmux notifications without active mux")?;
        let owner = self.registered_owner_weak()?;
        let baseline_gate = Arc::new(OnceLock::<bool>::new());
        let callback_baseline = Arc::clone(&baseline_gate);
        let (sub_id, _session_incarnation, baseline_revision) = mux
            .subscribe_with_topology_fence(move |envelope| {
                // A concurrently dispatched post-baseline event waits for
                // the one-time coordinator handoff instead of being dropped.
                // After publication, wait observes an initialized one-shot
                // latch; PaneOutput storms no longer contend on a handoff
                // mutex.
                if !*callback_baseline.wait() {
                    return false;
                }
                let Some(owner) = owner.upgrade() else {
                    return false;
                };
                owner.ingest_mux_notification(envelope)
            })
            .context("cannot allocate fenced tmux notification subscription")?;
        if let Err(err) = self
            .notification_intents
            .lock()
            .initialize_topology_order(baseline_revision)
        {
            let _ = mux.unsubscribe(sub_id);
            let _ = baseline_gate.set(false);
            return Err(err.context("cannot initialize tmux notification topology ordering"));
        }
        if baseline_gate.set(true).is_err() {
            let _ = mux.unsubscribe(sub_id);
            anyhow::bail!("tmux notification subscription handoff was published more than once");
        }

        match self.publish_notification_subscription(sub_id) {
            Ok(true) => Ok(()),
            Ok(false) => {
                let _ = mux.unsubscribe(sub_id);
                Ok(())
            }
            Err(err) => {
                let _ = mux.unsubscribe(sub_id);
                Err(err)
            }
        }
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

fn parse_split_pane_identity(output: &str) -> anyhow::Result<u64> {
    let mut identities = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    let identity = identities
        .next()
        .ok_or_else(|| anyhow!("split-window returned no pane id"))?;
    anyhow::ensure!(
        identities.next().is_none(),
        "split-window returned more than one pane identity"
    );
    let digits = identity
        .strip_prefix('%')
        .ok_or_else(|| anyhow!("split-window pane identity must begin with '%'"))?;
    anyhow::ensure!(
        !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()),
        "split-window pane identity must be exactly '%' followed by decimal digits"
    );
    digits
        .parse()
        .context("split-window pane identity is outside the supported range")
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
            let pane = match pane_map.get(&self.pane_id) {
                Some(x) => x.lock(),
                None => return "".to_string(),
            };

            if pane.pane_width == self.size.cols as u64 && pane.pane_height == self.size.rows as u64
            {
                return "".to_string();
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

        let mux = tmux_mux()?;
        let domain = mux
            .get_domain(domain_id)
            .ok_or_else(|| anyhow!("Tmux domain lost"))?;
        let tmux_domain = domain
            .downcast_ref::<TmuxDomain>()
            .ok_or_else(|| anyhow!("Tmux domain lost"))?;
        if let Some(pane) = tmux_domain.inner.remote_panes.lock().get(&self.pane_id) {
            let mut pane = pane.lock();
            pane.pane_width = self.size.cols as u64;
            pane.pane_height = self.size.rows as u64;
        }
        Ok(())
    }

    fn expected_responses(&self) -> usize {
        2
    }

    fn try_merge_newer(&mut self, newer: &dyn TmuxCommand) -> bool {
        let Some((pane_id, size)) = newer.as_resize() else {
            return false;
        };
        if pane_id != self.pane_id {
            return false;
        }

        self.size = size;
        true
    }

    fn as_resize(&self) -> Option<(TmuxPaneId, PtySize)> {
        Some((self.pane_id, self.size))
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
        let pane = pane_map.get(&self.pane_id).with_context(|| {
            format!("capture result targeted missing tmux pane {}", self.pane_id)
        })?;
        let mut pane = pane.lock();
        if pane.output_state != TmuxPaneOutputState::AwaitingCapture {
            anyhow::bail!(
                "capture result for tmux pane {} arrived in {:?} state",
                self.pane_id,
                pane.output_state
            );
        }
        pane.output_write
            .write_all(unescaped.as_bytes())
            .context("writing capture pane result to output")?;
        pane.output_state = TmuxPaneOutputState::Captured;

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

    fn mailbox_payload_bytes(&self) -> usize {
        self.keys.len()
    }

    fn try_merge_newer(&mut self, newer: &dyn TmuxCommand) -> bool {
        let Some((pane, keys)) = newer.as_send_keys() else {
            return false;
        };
        if pane != self.pane {
            return false;
        }

        let Some(combined_len) = self.keys.len().checked_add(keys.len()) else {
            return false;
        };
        if combined_len > SEND_KEYS_MERGE_MAX_BYTES {
            return false;
        }

        self.keys.extend_from_slice(keys);
        true
    }

    fn as_send_keys(&self) -> Option<(TmuxPaneId, &[u8])> {
        Some((self.pane, &self.keys))
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
        drop(support_commands);

        let session = tmux_domain
            .inner
            .tmux_session
            .lock()
            .ok_or_else(|| anyhow!("tmux session disappeared during command discovery"))?;
        tmux_domain.inner.enqueue_required(
            Box::new(ListAllWindows {
                session_id: session,
                window_id: None,
            }),
            "post-list-commands window discovery",
        )
    }
}

#[derive(Debug)]
pub(crate) struct SplitPane {
    pub pane_id: TmuxPaneId,
    pub direction: SplitDirection,
    pub request_id: u64,
}

impl TmuxCommand for SplitPane {
    fn get_command(&self, _domain_id: DomainId) -> String {
        if self.direction == SplitDirection::Horizontal {
            format!(
                "split-window -P -F '#{{pane_id}}' -h -t %{}\n",
                self.pane_id
            )
        } else {
            format!(
                "split-window -P -F '#{{pane_id}}' -v -t %{}\n",
                self.pane_id
            )
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
                            .fail_pending_split(self.request_id, anyhow!(error.clone()));
                    }
                }
            }
            log::error!("{error}");
            anyhow::bail!("{error}");
        }

        let pane_id = parse_split_pane_identity(&result.output);

        let mux = tmux_mux()?;
        let domain = mux
            .get_domain(domain_id)
            .ok_or_else(|| anyhow!("Tmux domain lost"))?;
        let tmux_domain = domain
            .downcast_ref::<TmuxDomain>()
            .ok_or_else(|| anyhow!("Tmux domain lost"))?;

        match pane_id {
            Ok(pane_id) => {
                anyhow::ensure!(
                    tmux_domain
                        .inner
                        .resolve_pending_split(self.request_id, pane_id),
                    "missing pending tmux split request {}",
                    self.request_id
                );
                Ok(())
            }
            Err(err) => {
                let message = format!(
                    "split-window in domain={domain_id} returned invalid pane identity: {err}"
                );
                let _ = tmux_domain
                    .inner
                    .fail_pending_split(self.request_id, anyhow!(message.clone()));
                anyhow::bail!("{message}");
            }
        }
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

    fn try_merge_newer(&mut self, newer: &dyn TmuxCommand) -> bool {
        let Some(window_id) = newer.as_select_window() else {
            return false;
        };
        self.window_id = window_id;
        true
    }

    fn as_select_window(&self) -> Option<TmuxWindowId> {
        Some(self.window_id)
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

    fn try_merge_newer(&mut self, newer: &dyn TmuxCommand) -> bool {
        let Some(pane_id) = newer.as_select_pane() else {
            return false;
        };
        self.pane_id = pane_id;
        true
    }

    fn as_select_pane(&self) -> Option<TmuxPaneId> {
        Some(self.pane_id)
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
        mux.add_domain(&domain)
            .expect("test tmux domain should register");

        (guard, tmux_domain)
    }

    fn insert_test_remote_pane(
        tmux_domain: &TmuxDomain,
        remote_pane_id: TmuxPaneId,
        remote_pane: crate::tmux::RefTmuxRemotePane,
    ) {
        let local_pane_id = remote_pane.lock().local_pane_id;
        tmux_domain
            .inner
            .mirror_index
            .lock()
            .register_pane(local_pane_id, remote_pane_id)
            .expect("register test pane reverse index");
        assert!(
            tmux_domain
                .inner
                .remote_panes
                .lock()
                .insert(remote_pane_id, remote_pane)
                .is_none(),
            "test remote pane identity must be unique"
        );
    }

    #[test]
    fn remove_tmux_pane_state_entries_removes_requested_ids_only() {
        let (_guard, tmux_domain) = install_tmux_domain();
        let removed_child_state = Arc::new(TmuxChildState::new());
        let retained_child_state = Arc::new(TmuxChildState::new());
        let (_read_removed, write_removed) = filedescriptor::socketpair().expect("socketpair");
        let (_read_retained, write_retained) = filedescriptor::socketpair().expect("socketpair");
        insert_test_remote_pane(
            &tmux_domain,
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
                output_state: TmuxPaneOutputState::Ready,
            })),
        );
        let removed_gate = Arc::clone(
            tmux_domain
                .inner
                .remote_panes
                .lock()
                .get(&11)
                .expect("removed pane gate"),
        );
        insert_test_remote_pane(
            &tmux_domain,
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
                output_state: TmuxPaneOutputState::Ready,
            })),
        );

        let limits = crate::tmux::TmuxBacklogLimits::new(32, 128, 8);
        {
            let mut backlog = tmux_domain.inner.backlog.lock();
            backlog.append_with_limits(11, b"pane-11", limits);
            backlog.append_with_limits(22, b"pane-22", limits);
            backlog.append_with_limits(33, b"pane-33", limits);
        }

        let removed_local_ids = tmux_domain
            .inner
            .retire_tmux_pane_state_entries(&[11, 33])
            .expect("retire requested panes");

        assert_eq!(removed_local_ids, vec![101]);
        let remote_panes = tmux_domain.inner.remote_panes.lock();
        assert!(!remote_panes.contains_key(&11));
        assert!(remote_panes.contains_key(&22));
        drop(remote_panes);
        let backlog = tmux_domain.inner.backlog.lock();
        assert!(!backlog.contains(11));
        assert!(backlog.contains(22));
        assert!(!backlog.contains(33));
        drop(backlog);
        let retired_panes = tmux_domain.inner.retired_panes.lock();
        assert!(retired_panes.contains(&11));
        assert!(retired_panes.contains(&33));
        assert!(!retired_panes.contains(&22));
        drop(retired_panes);
        assert_eq!(
            removed_gate.lock().output_state,
            TmuxPaneOutputState::Retired,
            "a producer that cloned the pane gate before removal must observe retirement"
        );
        tmux_domain.inner.advance(Box::new(vec![Event::Output {
            pane: 11,
            text: b"late".to_vec(),
        }]));
        assert!(
            !tmux_domain.inner.backlog.lock().contains(11),
            "late output for a tombstoned pane must not resurrect its backlog"
        );
        assert_eq!(
            removed_child_state
                .try_wait()
                .map(|status| status.exit_code()),
            Some(0)
        );
        assert!(retained_child_state.try_wait().is_none());
    }

    #[test]
    fn fresh_split_commit_preserves_prepublication_then_live_order() {
        use std::io::Read as _;

        let (_guard, tmux_domain) = install_tmux_domain();
        let (mut output_read, output_write) = filedescriptor::socketpair().expect("socketpair");
        insert_test_remote_pane(
            &tmux_domain,
            31,
            Arc::new(Mutex::new(TmuxRemotePane {
                local_pane_id: 131,
                output_write,
                child_state: Arc::new(TmuxChildState::new()),
                session_id: 1,
                window_id: 2,
                pane_id: 31,
                cursor_x: 0,
                cursor_y: 0,
                pane_width: 80,
                pane_height: 24,
                pane_left: 0,
                pane_top: 0,
                output_state: TmuxPaneOutputState::Fresh,
            })),
        );
        {
            let mut backlog = tmux_domain.inner.backlog.lock();
            let limits = crate::tmux::TmuxBacklogLimits::new(32, 128, 8);
            backlog.append_with_limits(31, b"A", limits);
            backlog.append_with_limits(31, b"B", limits);
        }

        tmux_domain
            .inner
            .finish_fresh_split(31)
            .expect("commit complete split stream");
        tmux_domain.inner.advance(Box::new(vec![Event::Output {
            pane: 31,
            text: b"C".to_vec(),
        }]));

        let mut observed = [0_u8; 3];
        output_read
            .read_exact(&mut observed)
            .expect("read committed and live output");
        assert_eq!(&observed, b"ABC");
        assert_eq!(
            tmux_domain
                .inner
                .remote_panes
                .lock()
                .get(&31)
                .expect("split pane")
                .lock()
                .output_state,
            TmuxPaneOutputState::Ready
        );
        assert!(!tmux_domain.inner.backlog.lock().contains(31));
    }

    #[test]
    fn gapped_fresh_split_never_becomes_ready() {
        let (_guard, tmux_domain) = install_tmux_domain();
        let (_output_read, output_write) = filedescriptor::socketpair().expect("socketpair");
        insert_test_remote_pane(
            &tmux_domain,
            32,
            Arc::new(Mutex::new(TmuxRemotePane {
                local_pane_id: 132,
                output_write,
                child_state: Arc::new(TmuxChildState::new()),
                session_id: 1,
                window_id: 2,
                pane_id: 32,
                cursor_x: 0,
                cursor_y: 0,
                pane_width: 80,
                pane_height: 24,
                pane_left: 0,
                pane_top: 0,
                output_state: TmuxPaneOutputState::Fresh,
            })),
        );
        tmux_domain.inner.backlog.lock().append_with_limits(
            32,
            b"AB",
            crate::tmux::TmuxBacklogLimits::new(1, 8, 8),
        );

        assert!(tmux_domain.inner.finish_fresh_split(32).is_err());
        assert_eq!(
            tmux_domain
                .inner
                .remote_panes
                .lock()
                .get(&32)
                .expect("split pane")
                .lock()
                .output_state,
            TmuxPaneOutputState::Fresh,
            "a gapped stream must never be published Ready"
        );
    }

    #[test]
    fn local_mirror_publication_failure_fences_remote_children_and_terminalizes() {
        let (_guard, tmux_domain) = install_tmux_domain();
        let child_state = Arc::new(TmuxChildState::new());
        let (_output_read, output_write) = filedescriptor::socketpair().expect("socketpair");
        insert_test_remote_pane(
            &tmux_domain,
            41,
            Arc::new(Mutex::new(TmuxRemotePane {
                local_pane_id: 141,
                output_write,
                child_state: Arc::clone(&child_state),
                session_id: 1,
                window_id: 2,
                pane_id: 41,
                cursor_x: 0,
                cursor_y: 0,
                pane_width: 80,
                pane_height: 24,
                pane_left: 0,
                pane_top: 0,
                output_state: TmuxPaneOutputState::Ready,
            })),
        );

        let err = tmux_domain
            .inner
            .fail_local_mirror_publication(anyhow!("duplicate local PaneId"));

        assert_eq!(err.to_string(), "duplicate local PaneId");
        assert!(tmux_domain.inner.is_terminal());
        assert!(
            child_state.try_wait().is_some(),
            "local rollback must prevent LocalPane::drop from sending kill-pane"
        );
        assert!(
            tmux_domain.inner.remote_panes.lock().is_empty(),
            "terminal cleanup must remove the partial remote-pane mirror"
        );
    }

    #[test]
    fn selection_commands_merge_latest_same_kind_without_crossing_kind_barriers() {
        let mut queue = crate::tmux::TmuxCmdQueue::new();
        queue
            .push_back(Box::new(SelectPane { pane_id: 1 }))
            .expect("first pane selection");
        queue
            .push_back(Box::new(SelectPane { pane_id: 2 }))
            .expect("newer pane selection");
        assert_eq!(queue.len(), 1);
        assert_eq!(
            queue.front().and_then(|command| command.as_select_pane()),
            Some(2)
        );

        queue
            .push_back(Box::new(SelectWindow { window_id: 3 }))
            .expect("window ordering barrier");
        queue
            .push_back(Box::new(SelectWindow { window_id: 4 }))
            .expect("newer window selection");
        assert_eq!(
            queue.len(),
            2,
            "same-kind latest-wins merging must not cross a pane/window ordering barrier"
        );
    }

    #[test]
    fn pane_output_storm_is_prefiltered_before_any_notification_runnable() {
        let (_guard, tmux_domain) = install_tmux_domain();
        *tmux_domain.inner.attach_state.lock() = AttachState::Done;

        for _ in 0..10_000 {
            assert!(tmux_domain.inner.ingest_mux_notification(
                MuxNotificationEnvelope {
                    notification: MuxNotification::PaneOutput(77),
                    topology: MuxTopologyStamp::NonTopology,
                }
            ));
        }

        let telemetry = tmux_domain.inner.notification_intent_telemetry.snapshot();
        assert_eq!(telemetry.received, 10_000);
        assert_eq!(telemetry.prefiltered, 10_000);
        assert_eq!(telemetry.scheduled, 0);
        assert_eq!(telemetry.applied, 0);
        assert_eq!(tmux_domain.inner.notification_intents.lock().pending_len(), 0);
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
    fn split_pane_identity_requires_one_exact_percent_id() {
        assert_eq!(parse_split_pane_identity("\n  %42  \n").unwrap(), 42);
        for malformed in [
            "",
            "$42",
            "@42",
            "%",
            "%+42",
            "%-1",
            "%42 suffix",
            "%42\n%43",
            "%18446744073709551616",
        ] {
            assert!(
                parse_split_pane_identity(malformed).is_err(),
                "malformed split identity {:?} must fail closed",
                malformed
            );
        }
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
    fn send_keys_merge_appends_same_pane_bytes_in_order() {
        let mut older = SendKeys {
            keys: vec![0x01, 0x02],
            pane: 7,
        };
        let newer = SendKeys {
            keys: vec![0x03, 0x04],
            pane: 7,
        };

        assert!(older.try_merge_newer(&newer));
        assert_eq!(older.keys, vec![0x01, 0x02, 0x03, 0x04]);
    }

    #[test]
    fn send_keys_merge_rejects_pane_and_command_type_mismatches() {
        let mut older = SendKeys {
            keys: vec![0x01, 0x02],
            pane: 7,
        };
        let different_pane = SendKeys {
            keys: vec![0x03, 0x04],
            pane: 8,
        };

        assert!(!older.try_merge_newer(&different_pane));
        assert!(!older.try_merge_newer(&ListCommands));
        assert_eq!(older.keys, vec![0x01, 0x02]);
    }

    #[test]
    fn send_keys_merge_accepts_exact_limit_and_rejects_overflow() {
        let mut older = SendKeys {
            keys: vec![0x01; SEND_KEYS_MERGE_MAX_BYTES - 1],
            pane: 7,
        };
        let at_limit = SendKeys {
            keys: vec![0x02],
            pane: 7,
        };
        let over_limit = SendKeys {
            keys: vec![0x03],
            pane: 7,
        };

        assert!(older.try_merge_newer(&at_limit));
        assert_eq!(older.keys.len(), SEND_KEYS_MERGE_MAX_BYTES);
        assert!(!older.try_merge_newer(&over_limit));
        assert_eq!(older.keys.len(), SEND_KEYS_MERGE_MAX_BYTES);
        assert_eq!(older.keys.last(), Some(&0x02));
    }

    #[test]
    fn resize_merge_uses_latest_same_pane_size() {
        let original = PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 8,
            pixel_height: 16,
        };
        let latest = PtySize {
            rows: 60,
            cols: 180,
            pixel_width: 10,
            pixel_height: 20,
        };
        let mut older = Resize {
            pane_id: 7,
            size: original,
        };
        let newer = Resize {
            pane_id: 7,
            size: latest,
        };

        assert!(older.try_merge_newer(&newer));
        assert_eq!(older.size, latest);
    }

    #[test]
    fn resize_merge_rejects_pane_and_command_type_mismatches() {
        let original = PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 8,
            pixel_height: 16,
        };
        let mut older = Resize {
            pane_id: 7,
            size: original,
        };
        let different_pane = Resize {
            pane_id: 8,
            size: PtySize {
                rows: 60,
                cols: 180,
                pixel_width: 10,
                pixel_height: 20,
            },
        };

        assert!(!older.try_merge_newer(&different_pane));
        assert!(!older.try_merge_newer(&ListCommands));
        assert_eq!(older.size, original);
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
            request_id: 1,
        };
        assert_eq!(
            cmd.get_command(0),
            "split-window -P -F '#{pane_id}' -h -t %5\n"
        );
    }

    #[test]
    fn split_pane_vertical_get_command() {
        let cmd = SplitPane {
            pane_id: 9,
            direction: SplitDirection::Vertical,
            request_id: 2,
        };
        assert_eq!(
            cmd.get_command(0),
            "split-window -P -F '#{pane_id}' -v -t %9\n"
        );
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
