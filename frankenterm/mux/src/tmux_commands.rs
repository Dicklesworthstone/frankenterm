use crate::domain::{DomainId, WriterWrapper};
use crate::localpane::LocalPane;
use crate::pane::{PaneId, alloc_pane_id};
use crate::tab::{SplitDirection, SplitRequest, SplitSize, Tab, TabId};
use crate::tmux::{
    AttachState, NOTIFICATION_INTENT_DRAIN_QUANTUM, TmuxBacklogDrain, TmuxBacklogLimits,
    TmuxDomain, TmuxDomainState, TmuxEnqueueError, TmuxNotificationIntent,
    TmuxNotificationIntentRunDisposition, TmuxPaneOutputIngress, TmuxPaneOutputLimits,
    TmuxPaneOutputState, TmuxRemotePane, TmuxSplitCleanupObligation, TmuxTab,
    TmuxTopologyBarrierEvent,
};
use crate::tmux_pty::{TmuxChild, TmuxChildState, TmuxPty};
use crate::{
    Mux, MuxNotification, MuxNotificationEnvelope, MuxTopologyStamp, Pane, PaneOperationGuard,
    SplitCommitReceipt,
};
use anyhow::{Context, anyhow};
use frankenterm_sigpipe::{RecoverablePanicSite, catch_recoverable};
use frankenterm_term::TerminalSize;
use parking_lot::Mutex;
use portable_pty::{ExitStatus, MasterPty, PtySize};
use std::collections::HashSet;
use std::convert::TryFrom;
use std::fmt::{Debug, Write};
use std::sync::{Arc, OnceLock};
use termwiz::tmux_cc::*;

/// Maximum payload retained in a single `SendKeys` command after queue merging.
const SEND_KEYS_MERGE_MAX_BYTES: usize = 16 * 1024;

#[cfg(test)]
const TEST_SPLIT_CONFIG_PANIC_GET: u8 = 1;
#[cfg(test)]
const TEST_SPLIT_CONFIG_PANIC_SET: u8 = 2;

/// Semantic admission and service class for a tmux control-mode command.
///
/// These classes are intentionally finite and explicit: mailbox capacity,
/// ordering, and scheduling policy must never depend on command text or a
/// best-effort downcast at the point where the queue is already saturated.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TmuxCommandClass {
    LosslessInput,
    RequiredControl,
    TerminalControl,
    CoalescibleIntent,
}

#[derive(Clone, Debug)]
pub(crate) enum TmuxSplitFailureAuthority {
    Baseline {
        request_id: u64,
    },
    Pending {
        request_id: u64,
        target_pane_id: TmuxPaneId,
    },
    Reconciliation {
        request_id: u64,
        target_pane_id: TmuxPaneId,
    },
    Compensation(Arc<TmuxSplitCleanupObligation>),
}

/// The suppression-cache target owned by a prepared tmux command.
///
/// This key is deliberately narrower than the command text. It lets the
/// mailbox publish a new intent generation at admission time, before command
/// preparation or I/O can race with a newer request for the same remote
/// object.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum TmuxConditionalCommitTarget {
    WindowLayout(TmuxWindowId),
    PaneSize(TmuxPaneId),
}

/// Exact fact whose publication can make a parked pure preparation useful.
///
/// Attach completion is a one-time domain-wide boundary. Pane publication is
/// deliberately target-scoped: an unrelated window/layout response must not
/// re-run every dormant resize in a large session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TmuxPreparationPrerequisite {
    Attach,
    Pane(TmuxPaneId),
}

/// The exact intent whose successful result may update a suppression cache.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TmuxConditionalCommitIntent {
    WindowLayout {
        window_id: TmuxWindowId,
        prune: bool,
        layout_csum: String,
    },
    PaneSize {
        pane_id: TmuxPaneId,
        rows: u16,
        cols: u16,
    },
}

impl TmuxConditionalCommitIntent {
    pub(crate) const fn target(&self) -> TmuxConditionalCommitTarget {
        match self {
            Self::WindowLayout { window_id, .. } => {
                TmuxConditionalCommitTarget::WindowLayout(*window_id)
            }
            Self::PaneSize { pane_id, .. } => TmuxConditionalCommitTarget::PaneSize(*pane_id),
        }
    }

    pub(crate) fn retained_bytes(&self) -> usize {
        match self {
            Self::WindowLayout { layout_csum, .. } => layout_csum.capacity(),
            Self::PaneSize { .. } => 0,
        }
    }
}

/// Mailbox-issued authority for one admitted conditional cache update.
///
/// Generations never wrap. A later admitted intent for the same target
/// replaces this lease in the mailbox even when the requested value happens
/// to be identical.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TmuxConditionalCommitLease {
    pub(crate) generation: u64,
    pub(crate) intent: TmuxConditionalCommitIntent,
}

/// Identity-fenced cache mutation carried beside immutable command bytes.
#[derive(Clone, Debug)]
pub(crate) enum TmuxConditionalCommit {
    WindowLayout {
        io_generation: u64,
        lease: TmuxConditionalCommitLease,
        local_tab_id: TabId,
    },
    PaneSize {
        io_generation: u64,
        lease: TmuxConditionalCommitLease,
        local_pane_id: PaneId,
        local_tab_id: TabId,
        remote_window_id: TmuxWindowId,
    },
}

impl TmuxConditionalCommit {
    pub(crate) const fn io_generation(&self) -> u64 {
        match self {
            Self::WindowLayout { io_generation, .. } | Self::PaneSize { io_generation, .. } => {
                *io_generation
            }
        }
    }

    pub(crate) const fn lease(&self) -> &TmuxConditionalCommitLease {
        match self {
            Self::WindowLayout { lease, .. } | Self::PaneSize { lease, .. } => lease,
        }
    }

    pub(crate) fn retained_bytes(&self) -> usize {
        std::mem::size_of_val(self).saturating_add(self.lease().intent.retained_bytes())
    }
}

/// Pure preparation result for one mailbox item.
///
/// `Ready` owns immutable command bytes and optional conditional commit
/// authority. `Suppressed` means the authoritative cache already matches the
/// request. `Retryable` means a known transient prerequisite is not ready yet;
/// no suppression state was changed, so the same admitted request remains
/// eligible after relevant progress. `Discarded` means the target or command
/// capability is definitively gone and retaining the request would only poison
/// bounded mailbox capacity.
#[derive(Debug)]
pub(crate) enum TmuxCommandPreparation {
    Ready {
        command: Vec<u8>,
        conditional_commit: Option<TmuxConditionalCommit>,
    },
    Suppressed,
    Retryable {
        prerequisite: TmuxPreparationPrerequisite,
    },
    Discarded,
}

impl TmuxCommandClass {
    pub(crate) const COUNT: usize = 4;

    pub(crate) const fn index(self) -> usize {
        match self {
            Self::LosslessInput => 0,
            Self::RequiredControl => 1,
            Self::TerminalControl => 2,
            Self::CoalescibleIntent => 3,
        }
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::LosslessInput => "lossless_input",
            Self::RequiredControl => "required_control",
            Self::TerminalControl => "terminal_control",
            Self::CoalescibleIntent => "coalescible_intent",
        }
    }
}

pub(crate) trait TmuxCommand: Send + Debug {
    fn mailbox_class(&self) -> TmuxCommandClass;
    fn get_command(&self, domain_id: DomainId) -> String;
    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()>;

    /// Intent whose cache update must be fenced against newer admissions.
    fn conditional_commit_intent(&self) -> Option<TmuxConditionalCommitIntent> {
        None
    }

    /// Prepare immutable command bytes. Commands with conditional suppression
    /// state override this method with a side-effect-free inspection path.
    /// The default preserves the historical behavior of dropping an empty
    /// non-conditional command rather than reordering a lossless/control lane.
    fn prepare(
        &mut self,
        domain_id: DomainId,
        _io_generation: u64,
        _lease: Option<TmuxConditionalCommitLease>,
    ) -> TmuxCommandPreparation {
        let command = self.get_command(domain_id);
        if command.is_empty() {
            TmuxCommandPreparation::Discarded
        } else {
            TmuxCommandPreparation::Ready {
                command: command.into_bytes(),
                conditional_commit: None,
            }
        }
    }

    /// Whether a successful guarded response is only the first terminal
    /// boundary for this command.
    ///
    /// `detach-client` is serialized through the ordinary command mailbox so
    /// its guarded response cannot be confused with another command. The I/O
    /// supervisor must then retain the same generation lease until tmux emits
    /// its clean `Exit` event.
    fn awaits_clean_exit(&self) -> bool {
        false
    }

    /// Whether this command is part of the exact remote-split cleanup
    /// transaction. Terminalizing domains continue only these commands (and a
    /// response already in flight) until the remote identity is killed or
    /// explicitly quarantined.
    fn is_split_transaction(&self) -> bool {
        false
    }

    fn split_failure_authority(&self) -> Option<TmuxSplitFailureAuthority> {
        None
    }

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

/// Rollback authority for an existing remote pane whose local mirror is not
/// yet structurally published. Initial window synchronization discovers these
/// panes rather than spawning them, so rollback fences the local child without
/// sending a destructive `kill-pane` to the authoritative tmux session.
struct TmuxInitialPanePublication<'owner> {
    owner: &'owner TmuxDomainState,
    pane: Arc<dyn Pane>,
    remote_gate: crate::tmux::RefTmuxRemotePane,
    remote_pane_id: TmuxPaneId,
    remote_window_id: TmuxWindowId,
    local_pane_id: PaneId,
    armed: bool,
}

impl TmuxInitialPanePublication<'_> {
    fn pane(&self) -> &Arc<dyn Pane> {
        &self.pane
    }

    fn commit(mut self) {
        self.armed = false;
    }
}

impl Drop for TmuxInitialPanePublication<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.owner.rollback_initial_pane_publication(self);
        }
    }
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
        let mut seen_pane_ids = HashSet::with_capacity(pane_ids.len());
        let pane_ids = pane_ids
            .iter()
            .copied()
            .filter(|pane_id| seen_pane_ids.insert(*pane_id))
            .collect::<Vec<_>>();
        let _retirement = self.pane_registry.lock();
        let removed_panes = {
            let mut remote_panes = self.remote_panes.lock();
            let mut retired_panes = self.retired_panes.lock();
            let remote_split_reservations = self.remote_split_reservations.lock();
            let mut mirror_index = self.mirror_index.lock();
            let new_tombstones = pane_ids
                .iter()
                .filter(|pane_id| {
                    !retired_panes.contains(pane_id)
                        && !remote_split_reservations.contains_key(pane_id)
                })
                .count();
            let Some(next_tombstone_count) = retired_panes
                .len()
                .checked_add(remote_split_reservations.len())
                .and_then(|count| {
                    count.checked_add(self.remote_split_identity_permit_count_locked())
                })
                .and_then(|count| count.checked_add(new_tombstones))
            else {
                anyhow::bail!("tmux retired-pane tombstone accounting overflow");
            };
            anyhow::ensure!(
                next_tombstone_count <= super::tmux::RETIRED_PANE_TOMBSTONE_LIMIT,
                "tmux retired-pane tombstone cap {} exceeded",
                super::tmux::RETIRED_PANE_TOMBSTONE_LIMIT
            );

            for pane_id in &pane_ids {
                let remote_present = remote_panes.contains_key(pane_id);
                let indexed_present = mirror_index
                    .checked_local_pane_for_remote(*pane_id)?
                    .is_some();
                anyhow::ensure!(
                    remote_present == indexed_present,
                    "tmux pane {pane_id} map and reverse index disagree before retirement"
                );
                if let Some(reservation) = remote_split_reservations.get(pane_id) {
                    let state = reservation.load()?;
                    anyhow::ensure!(
                        matches!(
                            (remote_present, state),
                            (true, super::tmux::TmuxRemoteSplitState::Published)
                                | (false, super::tmux::TmuxRemoteSplitState::Retired)
                        ),
                        "tmux split pane {pane_id} has {state:?} reservation with remote_present={remote_present}"
                    );
                }
            }

            let mut removed = Vec::with_capacity(pane_ids.len());
            for pane_id in &pane_ids {
                let indexed_local = mirror_index.unregister_pane(*pane_id)?;
                if let Some(reservation) = remote_split_reservations.get(pane_id) {
                    match reservation.load()? {
                        super::tmux::TmuxRemoteSplitState::Published => reservation.transition(
                            super::tmux::TmuxRemoteSplitState::Published,
                            super::tmux::TmuxRemoteSplitState::Retired,
                        )?,
                        super::tmux::TmuxRemoteSplitState::Retired => {}
                        super::tmux::TmuxRemoteSplitState::Reserved => anyhow::bail!(
                            "tmux split pane {pane_id} retired before mirror publication"
                        ),
                    }
                } else {
                    retired_panes.insert(*pane_id);
                }
                let remote = remote_panes.remove(pane_id);
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
            remote.output_ingress.clear();
            remote
                .child_state
                .mark_exited(ExitStatus::with_exit_code(0));
            local_pane_ids.push(remote.local_pane_id);
        }

        // Every producer admitted before the tombstone has completed, and all
        // later output is rejected, before backlog cleanup.
        self.backlog.lock().remove_many(&pane_ids);
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

        let mut cmd_queue = self.cmd_queue.lock();
        for pane_id in &to_remove {
            cmd_queue.retire_pane_size_suppression_target(*pane_id);
            cmd_queue.advance_preparation_prerequisite(TmuxPreparationPrerequisite::Pane(*pane_id));
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

        let mut cmd_queue = self.cmd_queue.lock();
        for pane_id in detached_panes {
            cmd_queue.retire_pane_size_suppression_target(pane_id);
            cmd_queue.advance_preparation_prerequisite(TmuxPreparationPrerequisite::Pane(pane_id));
        }

        Ok(())
    }

    fn prepare_pane_capture(&self, pane_id: TmuxPaneId) -> anyhow::Result<bool> {
        let pane = self
            .remote_panes
            .lock()
            .get(&pane_id)
            .cloned()
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

    fn construct_pane(
        &self,
        pane: &PaneItem,
        child_state: Arc<TmuxChildState>,
    ) -> anyhow::Result<(Arc<dyn Pane>, crate::tmux::RefTmuxRemotePane)> {
        let local_pane_id = alloc_pane_id()?;
        let (output_read, mut output_write) = filedescriptor::socketpair()?;
        output_write
            .set_non_blocking(true)
            .context("making tmux pane output socket nonblocking")?;
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
            output_ingress: TmuxPaneOutputIngress::default(),
        }));

        let owner = self.registered_owner_weak()?;
        let pane_pty = TmuxPty {
            domain_id: self.domain_id,
            reader: output_read,
            cmd_queue: Arc::clone(&self.cmd_queue),
            master_pane: Arc::clone(&ref_pane),
            owner: owner.clone(),
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
            owner,
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

        Ok((local_pane, ref_pane))
    }

    fn prepare_initial_pane_publication(
        &self,
        pane: &PaneItem,
    ) -> anyhow::Result<TmuxInitialPanePublication<'_>> {
        let child_state = Arc::new(TmuxChildState::new());
        let (local_pane, ref_pane) = self.construct_pane(pane, Arc::clone(&child_state))?;
        let local_pane_id = local_pane.pane_id();
        let result = (|| {
            let _registry = self.pane_registry.lock();
            let retired_panes = self.retired_panes.lock();
            anyhow::ensure!(
                !retired_panes.contains(&pane.pane_id),
                "tmux attempted to reuse retired remote pane id {}",
                pane.pane_id
            );
            let reservations = self.remote_split_reservations.lock();
            anyhow::ensure!(
                !reservations.contains_key(&pane.pane_id),
                "tmux attempted to reuse reserved remote split pane id {}",
                pane.pane_id
            );
            let mut pane_map = self.remote_panes.lock();
            anyhow::ensure!(
                !pane_map.contains_key(&pane.pane_id),
                "tmux remote pane {} is already mapped to a local pane",
                pane.pane_id
            );
            pane_map
                .try_reserve(1)
                .map_err(|error| anyhow!("reserve initial tmux pane mirror: {error}"))?;
            let mut mirror_index = self.mirror_index.lock();
            mirror_index
                .prepare_pane_registration(local_pane_id, pane.pane_id)
                .context("prepare initial tmux pane reverse index")?;
            let mut gui_tabs = self.gui_tabs.lock();
            let gui_tab = gui_tabs.get_mut(&pane.window_id).ok_or_else(|| {
                anyhow!(
                    "tmux window {} disappeared before initial pane publication",
                    pane.window_id
                )
            })?;
            anyhow::ensure!(
                !gui_tab.panes.contains(&pane.pane_id),
                "tmux pane {} is already attached to window {}",
                pane.pane_id,
                pane.window_id
            );
            gui_tab
                .panes
                .try_reserve(1)
                .map_err(|error| anyhow!("reserve initial tmux pane membership: {error}"))?;

            let prior = pane_map.insert(pane.pane_id, Arc::clone(&ref_pane));
            debug_assert!(prior.is_none());
            mirror_index.commit_pane_registration(local_pane_id, pane.pane_id);
            let inserted = gui_tab.panes.insert(pane.pane_id);
            debug_assert!(inserted);

            Ok(TmuxInitialPanePublication {
                owner: self,
                pane: local_pane,
                remote_gate: ref_pane,
                remote_pane_id: pane.pane_id,
                remote_window_id: pane.window_id,
                local_pane_id,
                armed: true,
            })
        })();

        if result.is_err() {
            child_state.mark_exited(ExitStatus::with_signal(
                "initial tmux pane mirror preparation failed",
            ));
        }
        result
    }

    fn rollback_initial_pane_publication(&self, publication: &TmuxInitialPanePublication<'_>) {
        let mut inconsistent = false;
        {
            let _registry = self.pane_registry.lock();
            let mut pane_map = self.remote_panes.lock();
            let removed = pane_map.remove(&publication.remote_pane_id);
            if !matches!(
                removed.as_ref(),
                Some(removed) if Arc::ptr_eq(removed, &publication.remote_gate)
            ) {
                inconsistent = true;
            }
            let mut mirror_index = self.mirror_index.lock();
            match mirror_index.unregister_pane(publication.remote_pane_id) {
                Ok(Some(local_pane_id)) if local_pane_id == publication.local_pane_id => {}
                _ => inconsistent = true,
            }
            let mut gui_tabs = self.gui_tabs.lock();
            let removed_membership = gui_tabs
                .get_mut(&publication.remote_window_id)
                .is_some_and(|tab| tab.panes.remove(&publication.remote_pane_id));
            if !removed_membership {
                inconsistent = true;
            }
            self.backlog
                .lock()
                .remove_many(&[publication.remote_pane_id]);
        }

        let mut remote = publication.remote_gate.lock();
        remote.child_state.mark_exited(ExitStatus::with_signal(
            "initial tmux pane publication rolled back",
        ));
        remote.output_state = TmuxPaneOutputState::Retired;
        remote.output_ingress.clear();
        drop(remote);

        if inconsistent {
            log::error!(
                "initial tmux pane {} rollback lost exact mirror ownership",
                publication.remote_pane_id
            );
        }
        self.transition_to_exit_and_schedule_detach();
    }

    #[cfg(test)]
    fn finish_fresh_split(&self, pane_id: TmuxPaneId) -> anyhow::Result<()> {
        let remote_gate = {
            let pane_map = self.remote_panes.lock();
            pane_map
                .get(&pane_id)
                .cloned()
                .with_context(|| format!("cannot finish missing split tmux pane {pane_id}"))?
        };
        let mut remote_pane = remote_gate.lock();
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
            Some(TmuxBacklogDrain::Bytes(chunks)) => {
                remote_pane
                    .output_ingress
                    .prepend(chunks, TmuxPaneOutputLimits::current())
                    .map_err(|gap| {
                        anyhow!(
                            "tmux split pane {pane_id} initial output exceeded its live queue: \
                             {gap:?}"
                        )
                    })?;
            }
            None => {}
        }
        remote_pane.output_state = TmuxPaneOutputState::Ready;
        drop(remote_pane);
        self.schedule_ready_pane_output(pane_id, &remote_gate)
    }

    pub fn split_pane(
        &self,
        mux: &Arc<Mux>,
        target: &PaneOperationGuard,
        remote: super::tmux::TmuxRemoteSplitReservation,
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
        anyhow::ensure!(
            self.mirror_index
                .lock()
                .remote_pane_for_local(target.pane_id())
                == Some(remote.target_remote_pane_id()),
            "tmux split target registration {} no longer maps to remote pane {}",
            target.pane_id(),
            remote.target_remote_pane_id()
        );
        let target_remote = self
            .remote_panes
            .lock()
            .get(&remote.target_remote_pane_id())
            .cloned()
            .with_context(|| {
                format!(
                    "tmux split target remote pane {} disappeared before materialization",
                    remote.target_remote_pane_id()
                )
            })?;
        anyhow::ensure!(
            target_remote.lock().window_id == remote_window_id,
            "tmux split target remote pane {} changed windows before materialization",
            remote.target_remote_pane_id()
        );

        let expected_domain = mux.get_domain(self.domain_id).with_context(|| {
            format!("tmux domain {} retired before split commit", self.domain_id)
        })?;
        let expected_tmux_domain = expected_domain
            .downcast_ref::<TmuxDomain>()
            .context("tmux domain registration changed concrete type before split commit")?;
        anyhow::ensure!(
            std::ptr::eq(expected_tmux_domain.inner.as_ref(), self),
            "tmux domain {} changed exact identity before split commit",
            self.domain_id
        );

        let p = PaneItem {
            session_id: 0,
            window_id: remote_window_id,
            pane_id: remote.remote_pane_id(),
            _pane_index: 0,
            cursor_x: 0,
            cursor_y: 0,
            pane_width: split_size.second.cols as u64,
            pane_height: split_size.second.rows as u64,
            pane_left: 0,
            pane_top: 0,
            pane_active: false,
        };

        let child_state = remote.child_state();
        let (pane, remote_gate) = self
            .construct_pane(&p, child_state)
            .context("failed to construct local tmux split pane")?;
        // Rebind the reservation after the local pane. On every unpublished
        // failure below it therefore retires the remote child and marks the
        // shared child state exited before LocalPane::drop consults its
        // signaller, preserving exactly-one compensation.
        let mut remote = remote;
        catch_recoverable(
            RecoverablePanicSite::MuxPaneCallback,
            std::panic::AssertUnwindSafe(|| {
                #[cfg(test)]
                if self
                    .test_split_config_panic
                    .compare_exchange(
                        TEST_SPLIT_CONFIG_PANIC_GET,
                        0,
                        std::sync::atomic::Ordering::AcqRel,
                        std::sync::atomic::Ordering::Acquire,
                    )
                    .is_ok()
                {
                    panic!("injected tmux split get_config callback panic");
                }
                if let Some(config) = target.with_pane(|pane| pane.get_config()) {
                    #[cfg(test)]
                    if self
                        .test_split_config_panic
                        .compare_exchange(
                            TEST_SPLIT_CONFIG_PANIC_SET,
                            0,
                            std::sync::atomic::Ordering::AcqRel,
                            std::sync::atomic::Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        panic!("injected tmux split set_config callback panic");
                    }
                    pane.set_config(config);
                }
            }),
        )
        .map_err(|_| anyhow!("tmux split pane configuration callback panicked"))?;
        remote
            .publish_local_mirror(Arc::clone(&remote_gate), pane.pane_id(), remote_window_id)
            .context("publish prepared tmux split mirror")?;
        remote
            .prepare_output_commit()
            .context("preflight tmux split output publication")?;
        #[cfg(test)]
        if self
            .test_retire_split_domain_before_local_commit
            .swap(false, std::sync::atomic::Ordering::AcqRel)
        {
            anyhow::ensure!(
                mux.domain_was_detached_if_guard(&expected_domain),
                "injected tmux split domain retirement was not admitted"
            );
        }
        let cleanup_obligation = remote.cleanup_obligation();
        let cleanup_publication = cleanup_obligation
            .begin_publication()
            .context("freeze tmux split cleanup authority across local commit")?;
        let registration = tab
            .commit_unregistered_split_pane(
                mux,
                &expected_domain,
                self.domain_id,
                local_window_id,
                &target.registration(),
                split_request,
                &pane,
                || remote.complete_structural_cut(cleanup_publication),
            )
            .context("commit atomic tmux split publication")?;
        remote.finish_committed_output();

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
        let backlog_limits = TmuxBacklogLimits::current();
        {
            let mut backlog = self.backlog.lock();
            backlog.refresh_limits(backlog_limits);
            if backlog.requires_global_resync() {
                anyhow::bail!(
                    "tmux output backlog lost pane identity; refusing partial per-window recovery"
                );
            }
        }

        for pane in panes.iter() {
            if pane.session_id != current_session
                || !self.check_pane_attached(pane.window_id, pane.pane_id)
            {
                continue;
            }

            let remote_gate = {
                let pane_map = self.remote_panes.lock();
                pane_map.get(&pane.pane_id).cloned().with_context(|| {
                    format!(
                        "tmux pane {} is attached but has no local remote-pane gate",
                        pane.pane_id
                    )
                })?
            };
            let mut remote_pane = remote_gate.lock();
            let local_pane = mux.get_pane(remote_pane.local_pane_id).with_context(|| {
                format!(
                    "tmux pane {} maps to missing local pane {}",
                    pane.pane_id, remote_pane.local_pane_id
                )
            })?;

            let backlog_drain = {
                let mut backlog = self.backlog.lock();
                anyhow::ensure!(
                    !backlog.requires_global_resync(),
                    "tmux pane {} cannot recover from a global output gap",
                    pane.pane_id
                );
                backlog.take(pane.pane_id)
            };
            let output_limits = TmuxPaneOutputLimits::current();
            let mut apply_snapshot_cursor = true;
            match (remote_pane.output_state, backlog_drain) {
                (_, Some(TmuxBacklogDrain::ResyncRequired)) => {
                    anyhow::bail!(
                        "tmux pane {} output backlog is gapped; refusing unsafe textual replay",
                        pane.pane_id
                    );
                }
                (TmuxPaneOutputState::Fresh, Some(TmuxBacklogDrain::Bytes(chunks))) => {
                    remote_pane
                        .output_ingress
                        .prepend(chunks, output_limits)
                        .map_err(|gap| {
                            anyhow!(
                                "tmux pane {} pre-attach stream exceeded its live queue: {gap:?}",
                                pane.pane_id
                            )
                        })?;
                    apply_snapshot_cursor = remote_pane.output_ingress.is_empty();
                }
                (TmuxPaneOutputState::Fresh, None) => {
                    apply_snapshot_cursor = remote_pane.output_ingress.is_empty();
                }
                (TmuxPaneOutputState::Captured, None)
                    if remote_pane.output_ingress.capture_raced() =>
                {
                    anyhow::bail!(
                        "tmux pane {} produced {} bytes while capture publication was pending; \
                         refusing cursor-ambiguous textual replay",
                        pane.pane_id,
                        remote_pane.output_ingress.queued_bytes()
                    );
                }
                (TmuxPaneOutputState::Captured, None) => {}
                (TmuxPaneOutputState::Captured, Some(TmuxBacklogDrain::Bytes(_))) => {
                    anyhow::bail!(
                        "captured tmux pane {} retained an impossible unknown-pane backlog",
                        pane.pane_id
                    );
                }
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
                    remote_pane
                        .output_ingress
                        .push_back(format!("\u{1b}[{row};{col}H").into_bytes(), output_limits)
                        .map_err(|gap| {
                            anyhow!(
                                "tmux pane {} cursor commit exceeded its live queue: {gap:?}",
                                pane.pane_id
                            )
                        })?;
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
            self.schedule_ready_pane_output(pane.pane_id, &remote_gate)?;

            if pane.pane_active {
                let local_tab_id = {
                    let gui_tabs = self.gui_tabs.lock();
                    let Some(local_tab) = gui_tabs.get(&pane.window_id) else {
                        anyhow::bail!("invalid tmux window id {}", pane.window_id);
                    };
                    local_tab.tab_id
                };

                // Tab selection synchronously invokes pane focus hooks and mux
                // subscribers. Never expose the gui-tab registry lock to that
                // re-entrant callback graph.
                if let Some(tab) = mux.get_tab(local_tab_id) {
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
        let gui_window_id = {
            let gui_window = self.gui_window.lock();
            match gui_window.as_ref() {
                Some(window) => **window,
                None => {
                    anyhow::bail!("No tmux gui created");
                }
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
            let expected_domain = mux.get_domain(self.domain_id).ok_or_else(|| {
                self.fail_local_mirror_publication(anyhow!(
                    "tmux domain {} retired before initial pane publication",
                    self.domain_id
                ))
            })?;
            let expected_tmux_domain =
                expected_domain
                    .downcast_ref::<TmuxDomain>()
                    .ok_or_else(|| {
                        self.fail_local_mirror_publication(anyhow!(
                            "tmux domain {} changed concrete type before initial pane publication",
                            self.domain_id
                        ))
                    })?;
            if !std::ptr::eq(expected_tmux_domain.inner.as_ref(), self) {
                return Err(self.fail_local_mirror_publication(anyhow!(
                    "tmux domain {} changed exact identity before initial pane publication",
                    self.domain_id
                )));
            }

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
                        let publication =
                            self.prepare_initial_pane_publication(&p).map_err(|err| {
                                self.fail_local_mirror_publication(
                                    err.context("failed to prepare initial tmux pane mirror"),
                                )
                            })?;
                        tab.commit_unregistered_root_pane(
                            &mux,
                            &expected_domain,
                            self.domain_id,
                            publication.pane(),
                        )
                        .map_err(|err| {
                            self.fail_local_mirror_publication(
                                err.context("failed to publish initial tmux root pane"),
                            )
                        })?;
                        publication.commit();
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
                    if !self.check_pane_attached(p.window_id, p.pane_id) {
                        let publication =
                            self.prepare_initial_pane_publication(&p).map_err(|err| {
                                self.fail_local_mirror_publication(
                                    err.context("failed to prepare initial tmux pane mirror"),
                                )
                            })?;
                        if tab.get_active_pane().is_none() {
                            tab.commit_unregistered_root_pane(
                                &mux,
                                &expected_domain,
                                self.domain_id,
                                publication.pane(),
                            )
                            .map_err(|err| {
                                self.fail_local_mirror_publication(
                                    err.context("failed to publish initial tmux root pane"),
                                )
                            })?;
                            split_pane_index = tab.get_active_idx();
                            publication.commit();
                            continue;
                        }

                        let target_pane = tab
                            .iter_panes_ignoring_zoom()
                            .into_iter()
                            .find(|positioned| positioned.index == split_pane_index)
                            .map(|positioned| positioned.pane)
                            .ok_or_else(|| {
                                self.fail_local_mirror_publication(anyhow!(
                                    "initial tmux split target index {split_pane_index} disappeared"
                                ))
                            })?;
                        let target_registration =
                            mux.capture_pane_registration(&target_pane).ok_or_else(|| {
                                self.fail_local_mirror_publication(anyhow!(
                                    "initial tmux split target {} lost its exact registration",
                                    target_pane.pane_id()
                                ))
                            })?;
                        let split_request = SplitRequest {
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
                        };
                        let (_registration, inserted_index) = tab
                            .commit_unregistered_unattached_split_pane(
                                &mux,
                                &expected_domain,
                                self.domain_id,
                                &target_registration,
                                split_request,
                                publication.pane(),
                            )
                            .map_err(|err| {
                                self.fail_local_mirror_publication(
                                    err.context("failed to publish initial tmux split pane"),
                                )
                            })?;
                        split_pane_index = next_split_pane_index(inserted_index);
                        publication.commit();
                    } else {
                        let remote_pane = {
                            let pane_map = self.remote_panes.lock();
                            pane_map.get(&p.pane_id).cloned()
                        };
                        let remote_pane = match remote_pane {
                            Some(remote_pane) => remote_pane,
                            None => anyhow::bail!("cannot find the local pane for {}", p.pane_id),
                        };
                        let local_pane_id = remote_pane.lock().local_pane_id;

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

            mux.add_tab_to_window(&tab, gui_window_id).map_err(|err| {
                self.fail_local_mirror_publication(
                    err.context("failed to publish tmux tab in local mux window"),
                )
            })?;
            self.notify_gui_window_without_registry_lock(gui_window_id)
                .map_err(|err| {
                    self.fail_local_mirror_publication(
                        err.context("failed to publish tmux GUI window notification"),
                    )
                })?;

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

    /// Publish the retained GUI window without exposing `gui_window` to the
    /// synchronous mux subscriber graph. `MuxWindowBuilder::notify` flushes
    /// `WindowCreated` immediately on the main thread; the tmux subscriber
    /// re-enters `gui_window` while classifying that topology edge.
    fn notify_gui_window_without_registry_lock(
        &self,
        expected_window_id: crate::WindowId,
    ) -> anyhow::Result<()> {
        let mut builder = self
            .gui_window
            .lock()
            .take()
            .context("tmux GUI window disappeared before creation notification")?;
        anyhow::ensure!(
            *builder == expected_window_id,
            "tmux GUI window changed identity before creation notification"
        );
        builder.notify();
        let mut slot = self.gui_window.lock();
        anyhow::ensure!(
            slot.is_none(),
            "tmux GUI window was replaced during creation notification"
        );
        *slot = Some(builder);
        Ok(())
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

    fn ingest_mux_notification(self: &Arc<Self>, envelope: MuxNotificationEnvelope) -> bool {
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
            MuxNotification::WindowTopologyChanged(change) => {
                let local_window_id = self
                    .gui_window
                    .lock()
                    .as_ref()
                    .map(|window| window.window_id);
                if local_window_id.is_some_and(|window_id| change.affects_window(window_id)) {
                    let window_id = local_window_id.expect("checked local window identity");
                    TmuxTopologyBarrierEvent::Intent(TmuxNotificationIntent::WindowInvalidated(
                        window_id,
                    ))
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
                        metrics::counter!("mux.tmux.notification_intent.ordering_failures")
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

    fn run_notification_intent_runnable(self: &Arc<Self>) -> TmuxNotificationIntentRunDisposition {
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

    fn drain_notification_intents(&self, mux: &Arc<Mux>) -> TmuxNotificationIntentRunDisposition {
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

                let Some(command) = self.resolve_notification_intent_command(mux, sequenced.intent)
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
                        if let Err(err) =
                            self.require_send_schedule("notification-intent admission")
                        {
                            log::error!("{err:#}");
                            self.notification_intents.lock().close();
                            return TmuxNotificationIntentRunDisposition::Closed;
                        }
                    }
                    Err(TmuxEnqueueError::Full) => {
                        self.notification_intent_telemetry.record_backpressured();
                        return self.notification_intents.lock().finish_quantum();
                    }
                    Err(TmuxEnqueueError::Closed) => {
                        self.notification_intents.lock().close();
                        return TmuxNotificationIntentRunDisposition::Closed;
                    }
                    Err(TmuxEnqueueError::ClassMismatch) => {
                        log::error!(
                            "tmux domain {} classified a notification intent outside the \
                             coalescible mailbox lane; detaching",
                            self.domain_id
                        );
                        self.transition_to_exit_and_schedule_detach();
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

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum SplitPaneIdentityParse {
    Exact(TmuxPaneId),
    RecoverableTrailingOutput {
        pane_id: TmuxPaneId,
        diagnostic: String,
    },
    Unresolved(String),
}

fn parse_exact_split_pane_identity(identity: &str) -> anyhow::Result<TmuxPaneId> {
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

pub(crate) fn parse_split_pane_identity(output: &str) -> SplitPaneIdentityParse {
    let identities: Vec<_> = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let Some(identity) = identities.first() else {
        return SplitPaneIdentityParse::Unresolved("split-window returned no pane id".to_string());
    };
    let pane_id = match parse_exact_split_pane_identity(identity) {
        Ok(pane_id) => pane_id,
        Err(error) => return SplitPaneIdentityParse::Unresolved(error.to_string()),
    };
    if identities.len() == 1 {
        return SplitPaneIdentityParse::Exact(pane_id);
    }
    if identities[1..]
        .iter()
        .any(|line| parse_exact_split_pane_identity(line).is_ok())
    {
        return SplitPaneIdentityParse::Unresolved(
            "split-window returned multiple pane identities".to_string(),
        );
    }
    SplitPaneIdentityParse::RecoverableTrailingOutput {
        pane_id,
        diagnostic: format!(
            "split-window returned a valid pane identity followed by unexpected output: {:?}",
            &identities[1..]
        ),
    }
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

enum ListAllPanesPreparation {
    Ready {
        command: String,
        local_tab_id: TabId,
    },
    Suppressed,
    Discarded,
}

impl ListAllPanes {
    fn prepare_pure(&self, domain_id: DomainId) -> ListAllPanesPreparation {
        let Some(mux) = Mux::try_get() else {
            return ListAllPanesPreparation::Discarded;
        };
        let Some(domain) = mux.get_domain(domain_id) else {
            return ListAllPanesPreparation::Discarded;
        };
        let Some(tmux_domain) = domain.downcast_ref::<TmuxDomain>() else {
            return ListAllPanesPreparation::Discarded;
        };

        let gui_tabs = tmux_domain.inner.gui_tabs.lock();
        let Some(local_tab) = gui_tabs.get(&self.window_id) else {
            // A later window-discovery result owns reconstruction. Retaining a
            // list-panes request for a window with no local identity would let
            // a permanently closed window occupy required-control capacity.
            return ListAllPanesPreparation::Discarded;
        };
        if self.prune && local_tab.layout_csum == self.layout_csum {
            return ListAllPanesPreparation::Suppressed;
        }
        let local_tab_id = local_tab.tab_id;
        drop(gui_tabs);

        ListAllPanesPreparation::Ready {
            command: format!(
                "list-panes -F '#{{session_id}} #{{window_id}} #{{pane_id}} \
                #{{pane_index}} #{{cursor_x}} #{{cursor_y}} #{{pane_width}} #{{pane_height}} \
                #{{pane_left}} #{{pane_top}} #{{pane_active}}' -t @{}\n",
                self.window_id
            ),
            local_tab_id,
        }
    }
}

impl TmuxCommand for ListAllPanes {
    fn mailbox_class(&self) -> TmuxCommandClass {
        TmuxCommandClass::RequiredControl
    }

    fn get_command(&self, domain_id: DomainId) -> String {
        match self.prepare_pure(domain_id) {
            ListAllPanesPreparation::Ready { command, .. } => command,
            ListAllPanesPreparation::Suppressed | ListAllPanesPreparation::Discarded => {
                String::new()
            }
        }
    }

    fn conditional_commit_intent(&self) -> Option<TmuxConditionalCommitIntent> {
        if !self.prune {
            // Initial/non-pruning snapshots are mandatory synchronization,
            // not checksum-coalescible layout refreshes. They must never be
            // superseded by a racing LayoutChange, and they do not own cache
            // publication because sync_window_state already installed the
            // authoritative window checksum.
            return None;
        }
        Some(TmuxConditionalCommitIntent::WindowLayout {
            window_id: self.window_id,
            prune: self.prune,
            layout_csum: self.layout_csum.clone(),
        })
    }

    fn prepare(
        &mut self,
        domain_id: DomainId,
        io_generation: u64,
        lease: Option<TmuxConditionalCommitLease>,
    ) -> TmuxCommandPreparation {
        let conditional_lease = if self.prune {
            let Some(lease) = lease else {
                return TmuxCommandPreparation::Discarded;
            };
            let Some(intent) = self.conditional_commit_intent() else {
                return TmuxCommandPreparation::Discarded;
            };
            if lease.intent != intent {
                return TmuxCommandPreparation::Discarded;
            }
            Some(lease)
        } else {
            if lease.is_some() {
                return TmuxCommandPreparation::Discarded;
            }
            None
        };
        match self.prepare_pure(domain_id) {
            ListAllPanesPreparation::Ready {
                command,
                local_tab_id,
            } => TmuxCommandPreparation::Ready {
                command: command.into_bytes(),
                conditional_commit: conditional_lease.map(|lease| {
                    TmuxConditionalCommit::WindowLayout {
                        io_generation,
                        lease,
                        local_tab_id,
                    }
                }),
            },
            ListAllPanesPreparation::Suppressed => TmuxCommandPreparation::Suppressed,
            ListAllPanesPreparation::Discarded => TmuxCommandPreparation::Discarded,
        }
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
                // Every list-panes response is an authoritative size/cursor
                // snapshot for panes that still exist locally. Layout-change
                // reconciliation additionally prunes identities absent from
                // that snapshot; pruning alone would leave dimensions stale.
                tmux_domain.inner.sync_pane_state(&items)?;
                if self.prune {
                    tmux_domain
                        .inner
                        .remove_detached_pane(self.window_id, &pane_set)?;
                }
                let mut cmd_queue = tmux_domain.inner.cmd_queue.lock();
                for pane_id in pane_set {
                    cmd_queue.advance_preparation_prerequisite(TmuxPreparationPrerequisite::Pane(
                        pane_id,
                    ));
                }
                return Ok(());
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
    fn mailbox_class(&self) -> TmuxCommandClass {
        TmuxCommandClass::RequiredControl
    }

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

enum ResizePreparation {
    Ready {
        command: String,
        local_pane_id: PaneId,
        local_tab_id: TabId,
        remote_window_id: TmuxWindowId,
    },
    Suppressed,
    Retryable(TmuxPreparationPrerequisite),
    Discarded,
}

impl Resize {
    fn prepare_pure(&self, domain_id: DomainId) -> ResizePreparation {
        let Some(mux) = Mux::try_get() else {
            return ResizePreparation::Discarded;
        };
        let Some(domain) = mux.get_domain(domain_id) else {
            return ResizePreparation::Discarded;
        };
        let Some(tmux_domain) = domain.downcast_ref::<TmuxDomain>() else {
            return ResizePreparation::Discarded;
        };

        // Tmux control mode cannot safely accept resize traffic during the
        // initial synchronization phase. This is a retryable preparation
        // boundary, not authority to update the cached size.
        if *tmux_domain.inner.attach_state.lock() == AttachState::Init {
            return ResizePreparation::Retryable(TmuxPreparationPrerequisite::Attach);
        }

        // Once any resize for this pane may have reached tmux, cached
        // dimensions are not suppression authority until the exact current
        // matching success commits them. Snapshot the mailbox-owned fact
        // before taking topology locks; no lock is nested across the two.
        let pane_size_suppression_is_trustworthy = tmux_domain
            .inner
            .cmd_queue
            .lock()
            .pane_size_suppression_is_trustworthy(self.pane_id);

        let remote_pane = {
            let _registry = tmux_domain.inner.pane_registry.lock();
            let remote_panes = tmux_domain.inner.remote_panes.lock();
            match remote_panes.get(&self.pane_id).cloned() {
                Some(remote_pane) => remote_pane,
                None if tmux_domain
                    .inner
                    .retired_panes
                    .lock()
                    .contains(&self.pane_id) =>
                {
                    return ResizePreparation::Discarded;
                }
                None => {
                    return ResizePreparation::Retryable(TmuxPreparationPrerequisite::Pane(
                        self.pane_id,
                    ));
                }
            }
        };
        let (local_pane_id, remote_window_id) = {
            let pane = remote_pane.lock();
            if pane_size_suppression_is_trustworthy
                && pane.pane_width == u64::from(self.size.cols)
                && pane.pane_height == u64::from(self.size.rows)
            {
                return ResizePreparation::Suppressed;
            }
            (pane.local_pane_id, pane.window_id)
        };

        let local_tab_id = {
            let gui_tabs = tmux_domain.inner.gui_tabs.lock();
            let Some(local_tab) = gui_tabs.get(&remote_window_id) else {
                return ResizePreparation::Retryable(TmuxPreparationPrerequisite::Pane(
                    self.pane_id,
                ));
            };
            local_tab.tab_id
        };
        let Some(local_tab) = mux.get_tab(local_tab_id) else {
            return ResizePreparation::Retryable(TmuxPreparationPrerequisite::Pane(self.pane_id));
        };
        let window_size = local_tab.get_size();

        let support_commands = tmux_domain.inner.support_commands.lock();
        let command = if support_commands.contains_key("resize-window") {
            format!(
                "resize-window -x {} -y {} -t @{}\nresize-pane -x {} -y {} -t %{}\n",
                window_size.cols,
                window_size.rows,
                remote_window_id,
                self.size.cols,
                self.size.rows,
                self.pane_id
            )
        } else if let Some(refresh_client) = support_commands.get("refresh-client") {
            let separator = if refresh_client.contains("-C XxY") {
                'x'
            } else {
                ','
            };
            format!(
                "refresh-client -C {}{}{}\nresize-pane -x {} -y {} -t %{}\n",
                window_size.cols,
                separator,
                window_size.rows,
                self.size.cols,
                self.size.rows,
                self.pane_id
            )
        } else {
            // Command discovery completes before AttachDone. At this point an
            // absent resize-window/refresh-client capability is permanent for
            // the connected tmux server and must not occupy the intent lane.
            return ResizePreparation::Discarded;
        };
        drop(support_commands);

        ResizePreparation::Ready {
            command,
            local_pane_id,
            local_tab_id,
            remote_window_id,
        }
    }
}

impl TmuxCommand for Resize {
    fn mailbox_class(&self) -> TmuxCommandClass {
        TmuxCommandClass::CoalescibleIntent
    }

    fn get_command(&self, domain_id: DomainId) -> String {
        match self.prepare_pure(domain_id) {
            ResizePreparation::Ready { command, .. } => command,
            ResizePreparation::Suppressed
            | ResizePreparation::Retryable(_)
            | ResizePreparation::Discarded => String::new(),
        }
    }

    fn conditional_commit_intent(&self) -> Option<TmuxConditionalCommitIntent> {
        Some(TmuxConditionalCommitIntent::PaneSize {
            pane_id: self.pane_id,
            rows: self.size.rows,
            cols: self.size.cols,
        })
    }

    fn prepare(
        &mut self,
        domain_id: DomainId,
        io_generation: u64,
        lease: Option<TmuxConditionalCommitLease>,
    ) -> TmuxCommandPreparation {
        let Some(lease) = lease else {
            return TmuxCommandPreparation::Discarded;
        };
        let Some(intent) = self.conditional_commit_intent() else {
            return TmuxCommandPreparation::Discarded;
        };
        if lease.intent != intent {
            return TmuxCommandPreparation::Discarded;
        }
        match self.prepare_pure(domain_id) {
            ResizePreparation::Ready {
                command,
                local_pane_id,
                local_tab_id,
                remote_window_id,
            } => TmuxCommandPreparation::Ready {
                command: command.into_bytes(),
                conditional_commit: Some(TmuxConditionalCommit::PaneSize {
                    io_generation,
                    lease,
                    local_pane_id,
                    local_tab_id,
                    remote_window_id,
                }),
            },
            ResizePreparation::Suppressed => TmuxCommandPreparation::Suppressed,
            ResizePreparation::Retryable(prerequisite) => {
                TmuxCommandPreparation::Retryable { prerequisite }
            }
            ResizePreparation::Discarded => TmuxCommandPreparation::Discarded,
        }
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            let error = format!("resize-pane in domain={domain_id} failed: {result:#?}");
            log::error!("{error}");
            anyhow::bail!("{error}");
        }

        // Cache publication is deliberately separate from result validation.
        // The I/O generation, mailbox intent generation, and pane/tab identity
        // captured by `prepare` are committed by `TmuxDomainState` only after
        // this matching guarded success.
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
    fn mailbox_class(&self) -> TmuxCommandClass {
        TmuxCommandClass::RequiredControl
    }

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

        let pane = {
            let pane_map = tmux_domain.inner.remote_panes.lock();
            pane_map.get(&self.pane_id).cloned()
        }
        .with_context(|| format!("capture result targeted missing tmux pane {}", self.pane_id))?;
        let mut pane = pane.lock();
        if pane.output_state != TmuxPaneOutputState::AwaitingCapture {
            anyhow::bail!(
                "capture result for tmux pane {} arrived in {:?} state",
                self.pane_id,
                pane.output_state
            );
        }
        anyhow::ensure!(
            !pane.output_ingress.capture_raced() && pane.output_ingress.is_empty(),
            "tmux pane {} produced output while capture was in flight; capture-time stream \
             authority is ambiguous",
            self.pane_id
        );
        pane.output_ingress
            .push_back(unescaped.into_bytes(), TmuxPaneOutputLimits::current())
            .map_err(|gap| {
                anyhow!(
                    "capture for tmux pane {} exceeded its live output queue: {gap:?}",
                    self.pane_id
                )
            })?;
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
    fn mailbox_class(&self) -> TmuxCommandClass {
        TmuxCommandClass::LosslessInput
    }

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
    fn mailbox_class(&self) -> TmuxCommandClass {
        TmuxCommandClass::RequiredControl
    }

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

pub(crate) const TMUX_SPLIT_TOKEN_OPTION: &str = "@frankenterm_split_token";

/// Response-matched, window-scoped remote baseline taken before the split
/// effect.  Its bytes are owned before admission so a later result never has
/// to allocate command authority.
#[derive(Debug)]
pub(crate) struct SnapshotSplitPane {
    pub request_id: u64,
    pub target_pane_id: TmuxPaneId,
    command: Option<Vec<u8>>,
}

impl SnapshotSplitPane {
    pub(crate) fn new(
        request_id: u64,
        target_pane_id: TmuxPaneId,
        window_id: TmuxWindowId,
    ) -> Self {
        Self {
            request_id,
            target_pane_id,
            command: Some(
                format!(
                    "list-panes -t @{window_id} -F '#{{session_id}} #{{window_id}} #{{pane_id}} #{{{TMUX_SPLIT_TOKEN_OPTION}}}'\n"
                )
                .into_bytes(),
            ),
        }
    }
}

impl TmuxCommand for SnapshotSplitPane {
    fn mailbox_class(&self) -> TmuxCommandClass {
        TmuxCommandClass::RequiredControl
    }

    fn get_command(&self, _domain_id: DomainId) -> String {
        String::from_utf8_lossy(self.command.as_deref().unwrap_or_default()).into_owned()
    }

    fn prepare(
        &mut self,
        _domain_id: DomainId,
        _io_generation: u64,
        _lease: Option<TmuxConditionalCommitLease>,
    ) -> TmuxCommandPreparation {
        match self.command.take() {
            Some(command) => TmuxCommandPreparation::Ready {
                command,
                conditional_commit: None,
            },
            None => TmuxCommandPreparation::Discarded,
        }
    }

    fn is_split_transaction(&self) -> bool {
        true
    }

    fn split_failure_authority(&self) -> Option<TmuxSplitFailureAuthority> {
        Some(TmuxSplitFailureAuthority::Baseline {
            request_id: self.request_id,
        })
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        let mux = tmux_mux()?;
        let domain = mux
            .get_domain(domain_id)
            .ok_or_else(|| anyhow!("Tmux domain lost"))?;
        let tmux_domain = domain
            .downcast_ref::<TmuxDomain>()
            .ok_or_else(|| anyhow!("Tmux domain lost"))?;
        if result.error {
            let _ = tmux_domain.inner.fail_pending_split(
                self.request_id,
                anyhow!("scoped tmux split baseline failed: {result:#?}"),
            );
            anyhow::bail!("split baseline in domain={domain_id} failed: {result:#?}");
        }
        tmux_domain.inner.finish_split_baseline(
            self.request_id,
            self.target_pane_id,
            &result.output,
        )
    }
}

#[derive(Debug)]
pub(crate) struct SplitPane {
    pub pane_id: TmuxPaneId,
    pub request_id: u64,
    command: Option<Vec<u8>>,
}

impl SplitPane {
    pub(crate) fn new(
        domain_id: DomainId,
        pane_id: TmuxPaneId,
        direction: SplitDirection,
        request_id: u64,
    ) -> Self {
        let orientation = if direction == SplitDirection::Horizontal {
            "-h"
        } else {
            "-v"
        };
        let token = format!("ft-{domain_id}-{request_id}");
        Self {
            pane_id,
            request_id,
            command: Some(
                format!(
                    "split-window -P -F '#{{pane_id}}' {orientation} -t %{pane_id} \\; set-option -p {TMUX_SPLIT_TOKEN_OPTION} {token}\n"
                )
                .into_bytes(),
            ),
        }
    }
}

impl TmuxCommand for SplitPane {
    fn mailbox_class(&self) -> TmuxCommandClass {
        TmuxCommandClass::RequiredControl
    }

    fn get_command(&self, _domain_id: DomainId) -> String {
        String::from_utf8_lossy(self.command.as_deref().unwrap_or_default()).into_owned()
    }

    fn prepare(
        &mut self,
        _domain_id: DomainId,
        _io_generation: u64,
        _lease: Option<TmuxConditionalCommitLease>,
    ) -> TmuxCommandPreparation {
        match self.command.take() {
            Some(command) => TmuxCommandPreparation::Ready {
                command,
                conditional_commit: None,
            },
            None => TmuxCommandPreparation::Discarded,
        }
    }

    fn is_split_transaction(&self) -> bool {
        true
    }

    fn split_failure_authority(&self) -> Option<TmuxSplitFailureAuthority> {
        Some(TmuxSplitFailureAuthority::Pending {
            request_id: self.request_id,
            target_pane_id: self.pane_id,
        })
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        let pane_id = parse_split_pane_identity(&result.output);

        let mux = tmux_mux()?;
        let domain = mux
            .get_domain(domain_id)
            .ok_or_else(|| anyhow!("Tmux domain lost"))?;
        let tmux_domain = domain
            .downcast_ref::<TmuxDomain>()
            .ok_or_else(|| anyhow!("Tmux domain lost"))?;

        match pane_id {
            SplitPaneIdentityParse::Exact(pane_id) => {
                if tmux_domain
                    .inner
                    .pending_split_identity_is_known(self.request_id, pane_id)?
                {
                    tmux_domain.inner.begin_split_reconciliation(
                        self.request_id,
                        self.pane_id,
                        format!(
                            "split-window returned colliding pane identity %{pane_id}; reconciling exact request delta"
                        ),
                    )?;
                    return Ok(());
                }
                if result.error {
                    anyhow::ensure!(
                        tmux_domain.inner.compensate_pending_split_identity(
                            self.request_id,
                            self.pane_id,
                            pane_id,
                            "split effect succeeded before its token-assignment command failed"
                                .to_string(),
                        )?,
                        "missing pending tmux split request {}",
                        self.request_id
                    );
                } else {
                    anyhow::ensure!(
                        tmux_domain.inner.resolve_pending_split(
                            self.request_id,
                            self.pane_id,
                            pane_id
                        )?,
                        "missing pending tmux split request {}",
                        self.request_id
                    );
                }
                Ok(())
            }
            SplitPaneIdentityParse::RecoverableTrailingOutput {
                pane_id,
                diagnostic,
            } => {
                if tmux_domain
                    .inner
                    .pending_split_identity_is_known(self.request_id, pane_id)?
                {
                    tmux_domain.inner.begin_split_reconciliation(
                        self.request_id,
                        self.pane_id,
                        format!("{diagnostic}; witness %{pane_id} collides with retained identity"),
                    )?;
                    return Ok(());
                }
                anyhow::ensure!(
                    tmux_domain.inner.compensate_pending_split_identity(
                        self.request_id,
                        self.pane_id,
                        pane_id,
                        diagnostic,
                    )?,
                    "missing pending tmux split request {}",
                    self.request_id
                );
                Ok(())
            }
            SplitPaneIdentityParse::Unresolved(err) => {
                let message = format!(
                    "split-window in domain={domain_id} returned invalid pane identity: {err}"
                );
                tmux_domain.inner.begin_split_reconciliation(
                    self.request_id,
                    self.pane_id,
                    message,
                )?;
                Ok(())
            }
        }
    }
}

/// Request-scoped recovery after `split-window` output lost an unambiguous
/// identity. The pending request owns a pre-command identity snapshot; this
/// command may compensate only an exact one-element set difference.
#[derive(Debug)]
pub(crate) struct ReconcileSplitPane {
    pub request_id: u64,
    pub target_pane_id: TmuxPaneId,
    command: Option<Vec<u8>>,
}

impl ReconcileSplitPane {
    pub(crate) fn new(
        request_id: u64,
        target_pane_id: TmuxPaneId,
        window_id: TmuxWindowId,
    ) -> Self {
        Self {
            request_id,
            target_pane_id,
            command: Some(
                format!(
                    "list-panes -t @{window_id} -F '#{{session_id}} #{{window_id}} #{{pane_id}} #{{{TMUX_SPLIT_TOKEN_OPTION}}}'\n"
                )
                .into_bytes(),
            ),
        }
    }
}

impl TmuxCommand for ReconcileSplitPane {
    fn mailbox_class(&self) -> TmuxCommandClass {
        TmuxCommandClass::RequiredControl
    }

    fn get_command(&self, _domain_id: DomainId) -> String {
        String::from_utf8_lossy(self.command.as_deref().unwrap_or_default()).into_owned()
    }

    fn prepare(
        &mut self,
        _domain_id: DomainId,
        _io_generation: u64,
        _lease: Option<TmuxConditionalCommitLease>,
    ) -> TmuxCommandPreparation {
        match self.command.take() {
            Some(command) => TmuxCommandPreparation::Ready {
                command,
                conditional_commit: None,
            },
            None => TmuxCommandPreparation::Discarded,
        }
    }

    fn is_split_transaction(&self) -> bool {
        true
    }

    fn split_failure_authority(&self) -> Option<TmuxSplitFailureAuthority> {
        Some(TmuxSplitFailureAuthority::Reconciliation {
            request_id: self.request_id,
            target_pane_id: self.target_pane_id,
        })
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        let mux = tmux_mux()?;
        let domain = mux
            .get_domain(domain_id)
            .ok_or_else(|| anyhow!("Tmux domain lost"))?;
        let tmux_domain = domain
            .downcast_ref::<TmuxDomain>()
            .ok_or_else(|| anyhow!("Tmux domain lost"))?;
        if result.error {
            tmux_domain.inner.fail_split_reconciliation(
                self.request_id,
                "list-panes guarded error",
                Vec::new(),
            );
            anyhow::bail!("split reconciliation in domain={domain_id} failed: {result:#?}");
        }
        tmux_domain.inner.finish_split_reconciliation(
            self.request_id,
            self.target_pane_id,
            &result.output,
        )
    }
}

/// Exact response-matched remote compensation. The obligation owns the
/// one-shot state transition; guarded success or failure completes it before
/// terminal queue/I/O teardown is allowed.
#[derive(Debug)]
pub(crate) struct CompensateSplitPane {
    pub obligation: Arc<TmuxSplitCleanupObligation>,
}

impl TmuxCommand for CompensateSplitPane {
    fn mailbox_class(&self) -> TmuxCommandClass {
        TmuxCommandClass::TerminalControl
    }

    fn get_command(&self, _domain_id: DomainId) -> String {
        self.obligation
            .pane_id()
            .map_or_else(String::new, |pane_id| format!("kill-pane -t %{pane_id}\n"))
    }

    fn prepare(
        &mut self,
        _domain_id: DomainId,
        _io_generation: u64,
        _lease: Option<TmuxConditionalCommitLease>,
    ) -> TmuxCommandPreparation {
        match self.obligation.take_kill_command() {
            Ok(command) => TmuxCommandPreparation::Ready {
                command,
                conditional_commit: None,
            },
            Err(error) => {
                log::error!("cannot prepare tmux split compensation: {error:#}");
                TmuxCommandPreparation::Discarded
            }
        }
    }

    fn is_split_transaction(&self) -> bool {
        true
    }

    fn split_failure_authority(&self) -> Option<TmuxSplitFailureAuthority> {
        Some(TmuxSplitFailureAuthority::Compensation(Arc::clone(
            &self.obligation,
        )))
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            self.obligation
                .finish_claimed(false, "tmux split compensation failed");
            anyhow::bail!(
                "split compensation in domain={domain_id} failed for request {}: {result:#?}",
                self.obligation.request_id()
            );
        }
        self.obligation
            .finish_claimed(true, "tmux split compensation");
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct SelectWindow {
    pub window_id: TmuxWindowId,
}

impl TmuxCommand for SelectWindow {
    fn mailbox_class(&self) -> TmuxCommandClass {
        TmuxCommandClass::CoalescibleIntent
    }

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
    fn mailbox_class(&self) -> TmuxCommandClass {
        TmuxCommandClass::CoalescibleIntent
    }

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

/// Explicitly detach this control-mode client.
///
/// tmux accepts `detach` as an alias of `detach-client`. Keeping this as a
/// first-class mailbox command gives its guarded response exactly the same
/// generation and ordering guarantees as every other control command.
#[derive(Debug)]
pub(crate) struct DetachClient;

impl TmuxCommand for DetachClient {
    fn mailbox_class(&self) -> TmuxCommandClass {
        TmuxCommandClass::TerminalControl
    }

    fn get_command(&self, _domain_id: DomainId) -> String {
        "detach\n".to_string()
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            let error = format!("detach-client in domain={domain_id} failed: {result:#?}");
            log::error!("{error}");
            anyhow::bail!("{error}");
        }
        Ok(())
    }

    fn awaits_clean_exit(&self) -> bool {
        true
    }
}

#[derive(Debug)]
pub(crate) struct KillPane {
    pub pane_id: TmuxPaneId,
    pub child_state: Arc<TmuxChildState>,
}

impl TmuxCommand for KillPane {
    fn mailbox_class(&self) -> TmuxCommandClass {
        TmuxCommandClass::TerminalControl
    }

    fn get_command(&self, _domain_id: DomainId) -> String {
        format!("kill-pane -t %{}\n", self.pane_id)
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            let error = format!("kill-pane in domain={domain_id} failed: {result:#?}");
            self.child_state
                .mark_exited(ExitStatus::with_signal("tmux kill-pane failed"));
            log::error!("{error}");
            anyhow::bail!("{error}");
        }
        self.child_state
            .mark_exited(ExitStatus::with_signal("tmux kill-pane"));
        Ok(())
    }
}

// This is a dummy command which indicates the attaching is done, it prevents the tmux output
// the unexpected and unnecessary content when syncing with back end in attaching stage.
#[derive(Debug)]
pub(crate) struct AttachDone;
impl TmuxCommand for AttachDone {
    fn mailbox_class(&self) -> TmuxCommandClass {
        TmuxCommandClass::RequiredControl
    }

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
        tmux_domain
            .inner
            .cmd_queue
            .lock()
            .advance_preparation_prerequisite(TmuxPreparationPrerequisite::Attach);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Domain;
    use filedescriptor::{AsRawSocketDescriptor, FileDescriptor, POLLIN, poll, pollfd};
    use promise::spawn::ScopedExecutor;
    use std::io::{Read as _, Write as _};
    use std::sync::MutexGuard as StdMutexGuard;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

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

        let tmux_domain =
            Arc::new(TmuxDomain::new(0).expect("start tmux test domain I/O supervisor"));
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
        let local_pane_id = {
            let mut pane = remote_pane.lock();
            pane.output_write
                .set_non_blocking(true)
                .expect("make test tmux output socket nonblocking");
            pane.local_pane_id
        };
        tmux_domain
            .inner
            .mirror_index
            .lock()
            .register_test_pane(local_pane_id, remote_pane_id)
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

    fn register_unattached_tmux_test_tab(
        mux: &Arc<Mux>,
        tmux_domain: &TmuxDomain,
        remote_window_id: TmuxWindowId,
    ) -> Arc<Tab> {
        let tab = Arc::new(Tab::new(&TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
            dpi: 0,
        }));
        mux.add_tab_no_panes(&tab)
            .expect("register empty tmux test tab");
        tmux_domain
            .inner
            .mirror_index
            .lock()
            .register_window(tab.tab_id(), remote_window_id)
            .expect("register tmux test window reverse index");
        assert!(
            tmux_domain
                .inner
                .gui_tabs
                .lock()
                .insert(
                    remote_window_id,
                    TmuxTab {
                        tab_id: tab.tab_id(),
                        tmux_window_id: remote_window_id,
                        layout_csum: "test".to_string(),
                        panes: HashSet::new(),
                    },
                )
                .is_none(),
            "tmux test window identity must be unique"
        );
        tab
    }

    fn initial_test_pane(remote_window_id: TmuxWindowId, remote_pane_id: TmuxPaneId) -> PaneItem {
        PaneItem {
            session_id: 1,
            window_id: remote_window_id,
            pane_id: remote_pane_id,
            _pane_index: 0,
            cursor_x: 0,
            cursor_y: 0,
            pane_width: 80,
            pane_height: 24,
            pane_left: 0,
            pane_top: 0,
            pane_active: false,
        }
    }

    fn attached_tmux_split_target(
        remote_window_id: TmuxWindowId,
        target_remote_pane_id: TmuxPaneId,
        workspace: &str,
    ) -> (
        ScopedMux,
        Arc<TmuxDomain>,
        Arc<Mux>,
        Arc<Tab>,
        Arc<dyn Pane>,
        PaneOperationGuard,
    ) {
        let (guard, tmux_domain) = install_tmux_domain();
        let mux = tmux_mux().expect("active tmux test mux");
        let tab = register_unattached_tmux_test_tab(&mux, &tmux_domain, remote_window_id);
        let target_item = initial_test_pane(remote_window_id, target_remote_pane_id);
        let target_publication = tmux_domain
            .inner
            .prepare_initial_pane_publication(&target_item)
            .expect("prepare tmux callback-panic target");
        let target_pane = Arc::clone(target_publication.pane());
        let expected_domain = mux
            .get_domain(tmux_domain.domain_id())
            .expect("exact tmux callback-panic domain registration");
        tab.commit_unregistered_root_pane(
            &mux,
            &expected_domain,
            tmux_domain.domain_id(),
            &target_pane,
        )
        .expect("publish tmux callback-panic target");
        target_publication.commit();

        let mut window = mux.new_empty_window(Some(workspace.to_string()), None);
        let local_window_id = *window;
        mux.add_tab_to_window(&tab, local_window_id)
            .expect("attach tmux callback-panic target tab");
        window.notify();
        let target = mux
            .capture_pane_operation(target_pane.pane_id())
            .expect("capture exact tmux callback-panic target");
        (guard, tmux_domain, mux, tab, target_pane, target)
    }

    fn assert_split_config_callback_panic_isolated(
        panic_stage: u8,
        remote_window_id: TmuxWindowId,
        target_remote_pane_id: TmuxPaneId,
        failed_remote_pane_id: TmuxPaneId,
        succeeding_remote_pane_id: TmuxPaneId,
        workspace: &str,
    ) {
        let (_guard, tmux_domain, mux, tab, target_pane, target) =
            attached_tmux_split_target(remote_window_id, target_remote_pane_id, workspace);
        tmux_domain
            .inner
            .test_split_config_panic
            .store(panic_stage, Ordering::Release);
        let failed_reservation = tmux_domain
            .inner
            .reserve_test_remote_split(10, target_remote_pane_id, failed_remote_pane_id)
            .expect("reserve callback-panic tmux split");

        let error = tmux_domain
            .inner
            .split_pane(&mux, &target, failed_reservation, SplitRequest::default())
            .expect_err("injected config callback panic must reject publication");
        assert!(format!("{error:#}").contains("tmux split pane configuration callback panicked"));
        assert_eq!(
            tmux_domain
                .inner
                .test_split_config_panic
                .load(Ordering::Acquire),
            0,
            "the one-shot callback panic plant must be consumed"
        );
        assert!(
            !tmux_domain.inner.is_terminal(),
            "an isolated pane callback panic must not strand or retire the tmux domain"
        );
        assert_eq!(tab.iter_all_panes().len(), 1);
        assert!(Arc::ptr_eq(&tab.iter_all_panes()[0], &target_pane));
        assert!(
            !tmux_domain
                .inner
                .remote_panes
                .lock()
                .contains_key(&failed_remote_pane_id)
        );
        assert_eq!(
            tmux_domain
                .inner
                .mirror_index
                .lock()
                .checked_local_pane_for_remote(failed_remote_pane_id)
                .expect("coherent callback-panic reverse index"),
            None
        );
        assert!(
            !tmux_domain
                .inner
                .gui_tabs
                .lock()
                .get(&remote_window_id)
                .expect("callback-panic target window survives")
                .panes
                .contains(&failed_remote_pane_id)
        );
        assert_eq!(
            tmux_domain
                .inner
                .remote_split_reservations
                .lock()
                .get(&failed_remote_pane_id)
                .expect("callback-panic split tombstone")
                .load()
                .expect("valid callback-panic split state"),
            crate::tmux::TmuxRemoteSplitState::Retired
        );
        {
            let queue = tmux_domain.inner.cmd_queue.lock();
            assert_eq!(
                queue.len(),
                1,
                "callback panic must enqueue exactly one remote compensation"
            );
            assert_eq!(
                queue
                    .front()
                    .expect("callback-panic compensation")
                    .get_command(tmux_domain.domain_id()),
                format!("kill-pane -t %{failed_remote_pane_id}\n")
            );
        }

        let succeeding_reservation = tmux_domain
            .inner
            .reserve_test_remote_split(11, target_remote_pane_id, succeeding_remote_pane_id)
            .expect("reserve split after isolated callback panic");
        let receipt = tmux_domain
            .inner
            .split_pane(
                &mux,
                &target,
                succeeding_reservation,
                SplitRequest::default(),
            )
            .expect("tmux domain must continue after isolated callback panic");
        assert_eq!(tab.iter_all_panes().len(), 2);
        assert!(mux.get_pane(receipt.pane_id()).is_some());
        assert!(
            tmux_domain
                .inner
                .remote_panes
                .lock()
                .contains_key(&succeeding_remote_pane_id)
        );
        assert!(
            !tmux_domain.inner.is_terminal(),
            "successful follow-up publication proves domain progress"
        );
        assert_eq!(
            tmux_domain.inner.cmd_queue.lock().len(),
            1,
            "the successful follow-up must not duplicate compensation"
        );
    }

    fn assert_split_output_preflight_failure_isolated(
        failure_stage: u8,
        expected_error: &str,
        remote_window_id: TmuxWindowId,
        target_remote_pane_id: TmuxPaneId,
        failed_remote_pane_id: TmuxPaneId,
        succeeding_remote_pane_id: TmuxPaneId,
        workspace: &str,
    ) {
        let (_guard, tmux_domain, mux, tab, target_pane, target) =
            attached_tmux_split_target(remote_window_id, target_remote_pane_id, workspace);
        let local_window_id = target
            .exact_location()
            .expect("exact output-preflight target location")
            .1;
        let before_workspace_counts = mux.num_panes_by_workspace.read().clone();
        let before_window_count = mux
            .get_window(local_window_id)
            .expect("output-preflight target window")
            .structural_pane_count();
        tmux_domain
            .inner
            .test_split_output_failure
            .store(failure_stage, Ordering::Release);
        let failed_reservation = tmux_domain
            .inner
            .reserve_test_remote_split(12, target_remote_pane_id, failed_remote_pane_id)
            .expect("reserve output-preflight negative split");

        let error = tmux_domain
            .inner
            .split_pane(&mux, &target, failed_reservation, SplitRequest::default())
            .expect_err("injected output preflight failure must reject publication");
        assert!(format!("{error:#}").contains(expected_error));
        assert_eq!(
            tmux_domain
                .inner
                .test_split_output_failure
                .load(Ordering::Acquire),
            0,
            "the one-shot output preflight plant must be consumed"
        );
        assert!(
            !tmux_domain.inner.is_terminal(),
            "a pre-commit output rejection must leave the domain available"
        );
        assert_eq!(tab.iter_all_panes().len(), 1);
        assert!(Arc::ptr_eq(&tab.iter_all_panes()[0], &target_pane));
        assert_eq!(*mux.num_panes_by_workspace.read(), before_workspace_counts);
        assert_eq!(
            mux.get_window(local_window_id)
                .expect("output-preflight target window survives")
                .structural_pane_count(),
            before_window_count
        );
        assert!(
            !tmux_domain
                .inner
                .remote_panes
                .lock()
                .contains_key(&failed_remote_pane_id)
        );
        assert_eq!(
            tmux_domain
                .inner
                .mirror_index
                .lock()
                .checked_local_pane_for_remote(failed_remote_pane_id)
                .expect("coherent output-preflight reverse index"),
            None
        );
        assert!(
            !tmux_domain
                .inner
                .gui_tabs
                .lock()
                .get(&remote_window_id)
                .expect("output-preflight remote window survives")
                .panes
                .contains(&failed_remote_pane_id)
        );
        assert_eq!(
            tmux_domain
                .inner
                .remote_split_reservations
                .lock()
                .get(&failed_remote_pane_id)
                .expect("output-preflight split tombstone")
                .load()
                .expect("valid output-preflight split state"),
            crate::tmux::TmuxRemoteSplitState::Retired
        );
        {
            let queue = tmux_domain.inner.cmd_queue.lock();
            assert_eq!(
                queue.len(),
                1,
                "output preflight failure must enqueue exactly one compensation"
            );
            assert_eq!(
                queue
                    .front()
                    .expect("output-preflight compensation")
                    .get_command(tmux_domain.domain_id()),
                format!("kill-pane -t %{failed_remote_pane_id}\n")
            );
        }

        let succeeding_reservation = tmux_domain
            .inner
            .reserve_test_remote_split(13, target_remote_pane_id, succeeding_remote_pane_id)
            .expect("reserve split after output preflight rejection");
        let receipt = tmux_domain
            .inner
            .split_pane(
                &mux,
                &target,
                succeeding_reservation,
                SplitRequest::default(),
            )
            .expect("tmux domain must progress after output preflight rejection");
        assert_eq!(tab.iter_all_panes().len(), 2);
        assert!(mux.get_pane(receipt.pane_id()).is_some());
        assert!(
            tmux_domain
                .inner
                .remote_panes
                .lock()
                .contains_key(&succeeding_remote_pane_id)
        );
        assert!(!tmux_domain.inner.is_terminal());
        assert_eq!(
            tmux_domain.inner.cmd_queue.lock().len(),
            1,
            "successful follow-up must not add another compensation"
        );
    }

    fn read_exact_with_timeout(reader: &mut FileDescriptor, expected_len: usize) -> Vec<u8> {
        reader
            .set_non_blocking(true)
            .expect("make test tmux output reader nonblocking");
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut observed = vec![0_u8; expected_len];
        let mut offset = 0;
        while offset < expected_len {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "timed out after reading {}/{} tmux output bytes",
                offset,
                expected_len
            );
            let mut readiness = [pollfd {
                fd: reader.as_socket_descriptor(),
                events: POLLIN,
                revents: 0,
            }];
            let ready =
                poll(&mut readiness, Some(remaining)).expect("poll tmux output test socket");
            assert_eq!(
                ready, 1,
                "timed out after reading {}/{} tmux output bytes",
                offset, expected_len
            );
            assert_ne!(
                readiness[0].revents & POLLIN,
                0,
                "tmux output socket woke without readable data"
            );
            match reader.read(&mut observed[offset..]) {
                Ok(0) => panic!("tmux output socket closed before the complete stream"),
                Ok(read) => offset += read,
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(err) => panic!("reading tmux output test socket failed: {}", err),
            }
        }
        observed
    }

    fn saturate_nonblocking_socket(writer: &mut FileDescriptor) -> usize {
        const MAX_TEST_FILL_BYTES: usize = 64 * 1024 * 1024;

        let payload = [0_u8; 64 * 1024];
        let mut written = 0_usize;
        loop {
            assert!(
                written < MAX_TEST_FILL_BYTES,
                "test socket did not become backpressured within {} bytes",
                MAX_TEST_FILL_BYTES
            );
            match writer.write(&payload) {
                Ok(0) => panic!("test socket accepted a zero-byte saturation write"),
                Ok(count) => written = written.saturating_add(count),
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => return written,
                Err(err) => panic!("saturating test socket failed: {}", err),
            }
        }
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
                output_ingress: TmuxPaneOutputIngress::default(),
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
                output_ingress: TmuxPaneOutputIngress::default(),
            })),
        );

        let limits = crate::tmux::TmuxBacklogLimits::new(32, 128, 8);
        {
            let mut backlog = tmux_domain.inner.backlog.lock();
            backlog.append_owned_with_limits(11, b"pane-11".to_vec(), limits);
            backlog.append_owned_with_limits(22, b"pane-22".to_vec(), limits);
            backlog.append_owned_with_limits(33, b"pane-33".to_vec(), limits);
        }

        let removed_local_ids = tmux_domain
            .inner
            .retire_tmux_pane_state_entries(&[11, 11, 33])
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
    fn pane_retirement_preflight_never_half_removes_a_valid_earlier_pane() {
        let (_guard, tmux_domain) = install_tmux_domain();
        let (_read_first, write_first) = filedescriptor::socketpair().expect("socketpair");
        let (_read_second, write_second) = filedescriptor::socketpair().expect("socketpair");
        for (remote_pane_id, local_pane_id, output_write) in
            [(71, 171, write_first), (72, 172, write_second)]
        {
            insert_test_remote_pane(
                &tmux_domain,
                remote_pane_id,
                Arc::new(Mutex::new(TmuxRemotePane {
                    local_pane_id,
                    output_write,
                    child_state: Arc::new(TmuxChildState::new()),
                    session_id: 1,
                    window_id: 2,
                    pane_id: remote_pane_id,
                    cursor_x: 0,
                    cursor_y: 0,
                    pane_width: 80,
                    pane_height: 24,
                    pane_left: 0,
                    pane_top: 0,
                    output_state: TmuxPaneOutputState::Ready,
                    output_ingress: TmuxPaneOutputIngress::default(),
                })),
            );
        }
        assert_eq!(
            tmux_domain
                .inner
                .mirror_index
                .lock()
                .unregister_pane(72)
                .expect("create a one-sided test mirror"),
            Some(172)
        );

        let err = tmux_domain
            .inner
            .retire_tmux_pane_state_entries(&[71, 72])
            .expect_err("corrupt second identity must reject the whole retirement batch");
        assert!(err.to_string().contains("map and reverse index disagree"));
        let pane_map = tmux_domain.inner.remote_panes.lock();
        assert!(pane_map.contains_key(&71));
        assert!(pane_map.contains_key(&72));
        drop(pane_map);
        assert_eq!(
            tmux_domain
                .inner
                .mirror_index
                .lock()
                .remote_pane_for_local(171),
            Some(71),
            "preflight failure must preserve an earlier valid reverse-index edge"
        );
        let retired = tmux_domain.inner.retired_panes.lock();
        assert!(!retired.contains(&71));
        assert!(!retired.contains(&72));
    }

    #[test]
    fn fresh_split_commit_preserves_prepublication_then_live_order() {
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
                output_ingress: TmuxPaneOutputIngress::default(),
            })),
        );
        {
            let mut backlog = tmux_domain.inner.backlog.lock();
            let limits = crate::tmux::TmuxBacklogLimits::new(32, 128, 8);
            backlog.append_owned_with_limits(31, b"A".to_vec(), limits);
            backlog.append_owned_with_limits(31, b"B".to_vec(), limits);
        }

        tmux_domain
            .inner
            .finish_fresh_split(31)
            .expect("commit complete split stream");
        tmux_domain.inner.advance(Box::new(vec![Event::Output {
            pane: 31,
            text: b"C".to_vec(),
        }]));

        let observed = read_exact_with_timeout(&mut output_read, 3);
        assert_eq!(observed, b"ABC");
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
    fn backpressured_pane_does_not_stall_an_independent_ready_pane() {
        let (_guard, tmux_domain) = install_tmux_domain();
        let (slow_read, slow_write) = filedescriptor::socketpair().expect("slow socketpair");
        let (mut fast_read, fast_write) = filedescriptor::socketpair().expect("fast socketpair");
        let slow_gate = Arc::new(Mutex::new(TmuxRemotePane {
            local_pane_id: 151,
            output_write: slow_write,
            child_state: Arc::new(TmuxChildState::new()),
            session_id: 1,
            window_id: 2,
            pane_id: 51,
            cursor_x: 0,
            cursor_y: 0,
            pane_width: 80,
            pane_height: 24,
            pane_left: 0,
            pane_top: 0,
            output_state: TmuxPaneOutputState::Ready,
            output_ingress: TmuxPaneOutputIngress::default(),
        }));
        let fast_gate = Arc::new(Mutex::new(TmuxRemotePane {
            local_pane_id: 152,
            output_write: fast_write,
            child_state: Arc::new(TmuxChildState::new()),
            session_id: 1,
            window_id: 2,
            pane_id: 52,
            cursor_x: 0,
            cursor_y: 0,
            pane_width: 80,
            pane_height: 24,
            pane_left: 0,
            pane_top: 0,
            output_state: TmuxPaneOutputState::Ready,
            output_ingress: TmuxPaneOutputIngress::default(),
        }));
        insert_test_remote_pane(&tmux_domain, 51, Arc::clone(&slow_gate));
        insert_test_remote_pane(&tmux_domain, 52, Arc::clone(&fast_gate));

        let saturated_bytes = {
            let mut slow_pane = slow_gate.lock();
            saturate_nonblocking_socket(&mut slow_pane.output_write)
        };
        assert!(saturated_bytes > 0);

        tmux_domain.inner.advance(Box::new(vec![Event::Output {
            pane: 51,
            text: b"SLOW".to_vec(),
        }]));
        tmux_domain.inner.advance(Box::new(vec![Event::Output {
            pane: 52,
            text: b"FAST".to_vec(),
        }]));

        assert_eq!(
            read_exact_with_timeout(&mut fast_read, 4),
            b"FAST",
            "a full pane socket must not block protocol progress for another pane"
        );
        assert!(
            !tmux_domain.inner.is_terminal(),
            "ordinary per-pane WouldBlock must remain recoverable"
        );
        assert_eq!(
            slow_gate.lock().output_ingress.queued_bytes(),
            4,
            "the blocked pane must retain its complete ordered chunk"
        );
        drop(slow_read);
    }

    #[test]
    fn many_ready_panes_each_drain_their_exact_stream_without_cross_pane_mixup() {
        const PANE_COUNT: usize = 64;

        let (_guard, tmux_domain) = install_tmux_domain();
        let mut readers = Vec::with_capacity(PANE_COUNT);
        let mut events = Vec::with_capacity(PANE_COUNT);
        for index in 0..PANE_COUNT {
            let remote_pane_id = 1_000 + u64::try_from(index).expect("test pane index fits u64");
            let local_pane_id = 2_000 + index;
            let (output_read, output_write) =
                filedescriptor::socketpair().expect("many-pane socketpair");
            let remote_gate = Arc::new(Mutex::new(TmuxRemotePane {
                local_pane_id,
                output_write,
                child_state: Arc::new(TmuxChildState::new()),
                session_id: 1,
                window_id: 2,
                pane_id: remote_pane_id,
                cursor_x: 0,
                cursor_y: 0,
                pane_width: 80,
                pane_height: 24,
                pane_left: 0,
                pane_top: 0,
                output_state: TmuxPaneOutputState::Ready,
                output_ingress: TmuxPaneOutputIngress::default(),
            }));
            insert_test_remote_pane(&tmux_domain, remote_pane_id, remote_gate);

            let payload = format!("pane-{remote_pane_id}").into_bytes();
            readers.push((output_read, payload.clone()));
            events.push(Event::Output {
                pane: remote_pane_id,
                text: payload,
            });
        }

        tmux_domain.inner.advance(Box::new(events));

        for (mut reader, expected) in readers {
            assert_eq!(
                read_exact_with_timeout(&mut reader, expected.len()),
                expected,
                "each materialized pane must retain an independent ordered stream"
            );
        }
        assert!(
            !tmux_domain.inner.is_terminal(),
            "a many-pane ready burst within the lane cap must remain healthy"
        );
    }

    #[test]
    fn capture_race_is_rejected_and_retirement_discards_queued_output() {
        let (_guard, tmux_domain) = install_tmux_domain();
        let (_output_read, output_write) = filedescriptor::socketpair().expect("socketpair");
        let pane_gate = Arc::new(Mutex::new(TmuxRemotePane {
            local_pane_id: 161,
            output_write,
            child_state: Arc::new(TmuxChildState::new()),
            session_id: 1,
            window_id: 2,
            pane_id: 61,
            cursor_x: 0,
            cursor_y: 0,
            pane_width: 80,
            pane_height: 24,
            pane_left: 0,
            pane_top: 0,
            output_state: TmuxPaneOutputState::AwaitingCapture,
            output_ingress: TmuxPaneOutputIngress::default(),
        }));
        insert_test_remote_pane(&tmux_domain, 61, Arc::clone(&pane_gate));

        tmux_domain.inner.advance(Box::new(vec![Event::Output {
            pane: 61,
            text: b"race".to_vec(),
        }]));
        let capture = CapturePane {
            pane_id: 61,
            history_limit: 100,
        };
        let err = capture
            .process_result(
                tmux_domain.domain_id(),
                &Guarded {
                    error: false,
                    timestamp: 0,
                    number: 0,
                    flags: 0,
                    output: "snapshot\n".to_string(),
                },
            )
            .expect_err("capture/live-output race must fail closed");
        assert!(
            err.to_string()
                .contains("capture-time stream authority is ambiguous")
        );
        {
            let pane = pane_gate.lock();
            assert!(pane.output_ingress.capture_raced());
            assert_eq!(pane.output_ingress.queued_bytes(), 4);
            assert_eq!(pane.output_state, TmuxPaneOutputState::AwaitingCapture);
        }

        assert_eq!(
            tmux_domain
                .inner
                .retire_tmux_pane_state_entries(&[61])
                .expect("retire capture-raced pane"),
            vec![161]
        );
        let pane = pane_gate.lock();
        assert_eq!(pane.output_state, TmuxPaneOutputState::Retired);
        assert!(pane.output_ingress.is_empty());
        assert_eq!(pane.output_ingress.queued_bytes(), 0);
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
                output_ingress: TmuxPaneOutputIngress::default(),
            })),
        );
        tmux_domain.inner.backlog.lock().append_owned_with_limits(
            32,
            b"AB".to_vec(),
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
        let remote_gate = Arc::new(Mutex::new(TmuxRemotePane {
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
            output_ingress: TmuxPaneOutputIngress::default(),
        }));
        insert_test_remote_pane(&tmux_domain, 41, Arc::clone(&remote_gate));
        remote_gate
            .lock()
            .output_ingress
            .push_back(
                b"retained until cleanup".to_vec(),
                TmuxPaneOutputLimits::current(),
            )
            .expect("queue bounded output before terminal cleanup");

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
        let remote = remote_gate.lock();
        assert_eq!(remote.output_state, TmuxPaneOutputState::Retired);
        assert!(
            remote.output_ingress.is_empty(),
            "terminal cleanup must release pane output retained by surviving PTY references"
        );
        assert_eq!(remote.output_ingress.queued_bytes(), 0);
    }

    #[test]
    fn tmux_atomic_publication_root_count_failure_rolls_back_every_surface() {
        let (_guard, tmux_domain) = install_tmux_domain();
        let mux = tmux_mux().expect("active tmux test mux");
        let tab = register_unattached_tmux_test_tab(&mux, &tmux_domain, 2);
        let pane_item = initial_test_pane(2, 41);
        let publication = tmux_domain
            .inner
            .prepare_initial_pane_publication(&pane_item)
            .expect("prepare unpublished initial pane");
        let local_pane_id = publication.local_pane_id;
        let remote_gate = Arc::clone(&publication.remote_gate);
        let expected_domain = mux
            .get_domain(tmux_domain.domain_id())
            .expect("exact tmux domain registration");
        mux.fail_next_pane_count_preparation
            .store(true, Ordering::Release);

        let error = tab
            .commit_unregistered_root_pane(
                &mux,
                &expected_domain,
                tmux_domain.domain_id(),
                publication.pane(),
            )
            .expect_err("injected root pane-count failure must reject publication");
        assert!(
            format!("{error:#}").contains("injected atomic unregistered tiled pane publication")
        );
        drop(publication);

        assert!(tab.iter_all_panes().is_empty());
        assert!(mux.get_pane(local_pane_id).is_none());
        assert!(!tmux_domain.inner.remote_panes.lock().contains_key(&41));
        assert_eq!(
            tmux_domain
                .inner
                .mirror_index
                .lock()
                .checked_local_pane_for_remote(41)
                .expect("coherent reverse index"),
            None
        );
        assert!(
            tmux_domain
                .inner
                .gui_tabs
                .lock()
                .get(&2)
                .is_none_or(|tab| !tab.panes.contains(&41))
        );
        let remote = remote_gate.lock();
        assert_eq!(remote.output_state, TmuxPaneOutputState::Retired);
        assert!(remote.child_state.try_wait().is_some());
        assert!(remote.output_ingress.is_empty());
        drop(remote);
        assert!(tmux_domain.inner.is_terminal());
        assert!(
            tmux_domain
                .inner
                .cmd_queue
                .lock()
                .front()
                .is_none_or(|command| {
                    command.get_command(tmux_domain.domain_id()) != "kill-pane -t %41\n"
                })
        );
    }

    #[test]
    fn tmux_atomic_publication_root_retired_domain_guard_rejects_zero_publication() {
        let (_guard, tmux_domain) = install_tmux_domain();
        let mux = tmux_mux().expect("active tmux test mux");
        let tab = register_unattached_tmux_test_tab(&mux, &tmux_domain, 13);
        let pane_item = initial_test_pane(13, 131);
        let publication = tmux_domain
            .inner
            .prepare_initial_pane_publication(&pane_item)
            .expect("prepare retired-domain root pane");
        let local_pane_id = publication.local_pane_id;
        let remote_gate = Arc::clone(&publication.remote_gate);
        let expected_domain = mux
            .get_domain(tmux_domain.domain_id())
            .expect("capture exact root domain generation");
        assert!(
            mux.domain_was_detached_if_guard(&expected_domain),
            "retire the exact root domain generation before its final cut"
        );

        let error = tab
            .commit_unregistered_root_pane(
                &mux,
                &expected_domain,
                tmux_domain.domain_id(),
                publication.pane(),
            )
            .expect_err("retired root domain generation must reject publication");
        assert!(format!("{error:#}").contains("domain retired or changed identity"));
        drop(publication);

        assert!(tab.iter_all_panes().is_empty());
        assert!(mux.get_pane(local_pane_id).is_none());
        assert!(!tmux_domain.inner.remote_panes.lock().contains_key(&131));
        assert_eq!(
            tmux_domain
                .inner
                .mirror_index
                .lock()
                .checked_local_pane_for_remote(131)
                .expect("coherent retired-domain root reverse index"),
            None
        );
        assert!(
            tmux_domain
                .inner
                .gui_tabs
                .lock()
                .get(&13)
                .is_none_or(|tab| !tab.panes.contains(&131))
        );
        let remote = remote_gate.lock();
        assert_eq!(remote.output_state, TmuxPaneOutputState::Retired);
        assert!(remote.child_state.try_wait().is_some());
        assert!(remote.output_ingress.is_empty());
    }

    #[test]
    fn tmux_atomic_publication_split_count_failure_rolls_back_every_surface() {
        let (_guard, tmux_domain) = install_tmux_domain();
        let mux = tmux_mux().expect("active tmux test mux");
        let tab = register_unattached_tmux_test_tab(&mux, &tmux_domain, 3);
        let target_item = initial_test_pane(3, 51);
        let target_publication = tmux_domain
            .inner
            .prepare_initial_pane_publication(&target_item)
            .expect("prepare tmux split target");
        let target_pane = Arc::clone(target_publication.pane());
        let expected_domain = mux
            .get_domain(tmux_domain.domain_id())
            .expect("exact tmux domain registration");
        tab.commit_unregistered_root_pane(
            &mux,
            &expected_domain,
            tmux_domain.domain_id(),
            &target_pane,
        )
        .expect("publish tmux split target");
        target_publication.commit();

        let mut window = mux.new_empty_window(Some("tmux-split-rollback".to_string()), None);
        let local_window_id = *window;
        mux.add_tab_to_window(&tab, local_window_id)
            .expect("attach tmux split target tab");
        window.notify();
        let target = mux
            .capture_pane_operation(target_pane.pane_id())
            .expect("capture exact tmux split target");
        let before_workspace_counts = mux.num_panes_by_workspace.read().clone();
        let before_window_count = mux
            .get_window(local_window_id)
            .expect("tmux test window")
            .structural_pane_count();
        let reservation = tmux_domain
            .inner
            .reserve_test_remote_split(7, 51, 52)
            .expect("reserve spawned tmux pane");
        mux.fail_next_pane_count_preparation
            .store(true, Ordering::Release);

        let error = tmux_domain
            .inner
            .split_pane(&mux, &target, reservation, SplitRequest::default())
            .expect_err("injected split pane-count failure must reject publication");
        assert!(
            format!("{error:#}").contains("injected atomic unregistered tiled pane publication")
        );

        assert_eq!(tab.iter_all_panes().len(), 1);
        assert!(Arc::ptr_eq(&tab.iter_all_panes()[0], &target_pane));
        assert_eq!(*mux.num_panes_by_workspace.read(), before_workspace_counts);
        assert_eq!(
            mux.get_window(local_window_id)
                .expect("tmux target window survives")
                .structural_pane_count(),
            before_window_count
        );
        assert!(!tmux_domain.inner.remote_panes.lock().contains_key(&52));
        assert_eq!(
            tmux_domain
                .inner
                .mirror_index
                .lock()
                .checked_local_pane_for_remote(52)
                .expect("coherent split reverse index"),
            None
        );
        assert!(
            !tmux_domain
                .inner
                .gui_tabs
                .lock()
                .get(&3)
                .expect("target tmux window survives")
                .panes
                .contains(&52)
        );
        assert_eq!(
            tmux_domain
                .inner
                .remote_split_reservations
                .lock()
                .get(&52)
                .expect("rolled-back split tombstone")
                .load()
                .expect("valid split tombstone state"),
            crate::tmux::TmuxRemoteSplitState::Retired
        );
        let queue = tmux_domain.inner.cmd_queue.lock();
        assert_eq!(
            queue.len(),
            1,
            "split rollback must enqueue one compensation"
        );
        assert_eq!(
            queue
                .front()
                .expect("split rollback compensation")
                .get_command(tmux_domain.domain_id()),
            "kill-pane -t %52\n"
        );
    }

    #[test]
    fn tmux_atomic_publication_split_retired_domain_guard_rolls_back_every_surface() {
        let (_guard, tmux_domain, mux, tab, target_pane, target) =
            attached_tmux_split_target(14, 141, "tmux-split-retired-domain");
        let domain_fence = mux
            .get_domain(tmux_domain.domain_id())
            .expect("retain exact domain generation through negative assertions");
        let local_window_id = target
            .exact_location()
            .expect("retired-domain split target location")
            .1;
        let before_workspace_counts = mux.num_panes_by_workspace.read().clone();
        let before_window_count = mux
            .get_window(local_window_id)
            .expect("retired-domain target window")
            .structural_pane_count();
        let reservation = tmux_domain
            .inner
            .reserve_test_remote_split(14, 141, 142)
            .expect("reserve retired-domain split");
        tmux_domain
            .inner
            .test_retire_split_domain_before_local_commit
            .store(true, Ordering::Release);

        let error = tmux_domain
            .inner
            .split_pane(&mux, &target, reservation, SplitRequest::default())
            .expect_err("retired split domain generation must reject local publication");
        assert!(format!("{error:#}").contains("domain retired or changed identity"));
        assert!(
            !tmux_domain
                .inner
                .test_retire_split_domain_before_local_commit
                .load(Ordering::Acquire)
        );
        assert_eq!(tab.iter_all_panes().len(), 1);
        assert!(Arc::ptr_eq(&tab.iter_all_panes()[0], &target_pane));
        assert_eq!(*mux.num_panes_by_workspace.read(), before_workspace_counts);
        assert_eq!(
            mux.get_window(local_window_id)
                .expect("retired-domain target window survives")
                .structural_pane_count(),
            before_window_count
        );
        assert!(!tmux_domain.inner.remote_panes.lock().contains_key(&142));
        assert_eq!(
            tmux_domain
                .inner
                .mirror_index
                .lock()
                .checked_local_pane_for_remote(142)
                .expect("coherent retired-domain split reverse index"),
            None
        );
        assert!(
            tmux_domain
                .inner
                .gui_tabs
                .lock()
                .get(&14)
                .is_none_or(|window| !window.panes.contains(&142)),
            "retired-domain cleanup must not retain the rejected split pane"
        );
        assert!(
            !tmux_domain
                .inner
                .remote_split_reservations
                .lock()
                .contains_key(&142),
            "retired domain generation must release its remote reservation directory"
        );
        assert!(
            tmux_domain.inner.cmd_queue.lock().is_empty(),
            "retired domain teardown must not retain orphaned command authority"
        );
        drop(domain_fence);
    }

    #[test]
    fn tmux_atomic_publication_root_and_split_success_publish_once() {
        let (_guard, tmux_domain) = install_tmux_domain();
        let mux = tmux_mux().expect("active tmux test mux");
        let tab = register_unattached_tmux_test_tab(&mux, &tmux_domain, 4);
        let target_item = initial_test_pane(4, 61);
        let target_publication = tmux_domain
            .inner
            .prepare_initial_pane_publication(&target_item)
            .expect("prepare successful tmux root");
        let target_pane = Arc::clone(target_publication.pane());
        let expected_domain = mux
            .get_domain(tmux_domain.domain_id())
            .expect("exact tmux domain registration");
        tab.commit_unregistered_root_pane(
            &mux,
            &expected_domain,
            tmux_domain.domain_id(),
            &target_pane,
        )
        .expect("commit successful tmux root");
        target_publication.commit();

        let mut window = mux.new_empty_window(Some("tmux-split-success".to_string()), None);
        let local_window_id = *window;
        mux.add_tab_to_window(&tab, local_window_id)
            .expect("attach successful tmux root tab");
        window.notify();
        let target = mux
            .capture_pane_operation(target_pane.pane_id())
            .expect("capture successful tmux split target");
        let reservation = tmux_domain
            .inner
            .reserve_test_remote_split(8, 61, 62)
            .expect("reserve successful spawned split");

        let receipt = tmux_domain
            .inner
            .split_pane(&mux, &target, reservation, SplitRequest::default())
            .expect("commit successful spawned tmux split");
        let split_local_id = tmux_domain
            .inner
            .mirror_index
            .lock()
            .checked_local_pane_for_remote(62)
            .expect("coherent successful split reverse index")
            .expect("successful split reverse index");

        assert_eq!(receipt.pane_id(), split_local_id);
        assert_eq!(tab.iter_all_panes().len(), 2);
        assert!(mux.get_pane(split_local_id).is_some());
        assert_eq!(
            mux.get_window(local_window_id)
                .expect("successful tmux window")
                .structural_pane_count(),
            2
        );
        assert_eq!(
            mux.num_panes_by_workspace
                .read()
                .get("tmux-split-success")
                .copied(),
            Some(2)
        );
        assert!(
            tmux_domain
                .inner
                .gui_tabs
                .lock()
                .get(&4)
                .expect("successful remote tmux window")
                .panes
                .contains(&62)
        );
        assert_eq!(
            tmux_domain
                .inner
                .remote_split_reservations
                .lock()
                .get(&62)
                .expect("successful split reservation")
                .load()
                .expect("valid successful split state"),
            crate::tmux::TmuxRemoteSplitState::Published
        );
        assert_eq!(
            tmux_domain
                .inner
                .remote_panes
                .lock()
                .get(&62)
                .expect("successful split remote gate")
                .lock()
                .output_state,
            TmuxPaneOutputState::Ready
        );
        assert_eq!(tmux_domain.inner.cmd_queue.lock().len(), 0);
    }

    #[test]
    fn tmux_atomic_publication_split_revalidates_target_after_response() {
        let (_guard, tmux_domain) = install_tmux_domain();
        let mux = tmux_mux().expect("active tmux test mux");
        let tab = register_unattached_tmux_test_tab(&mux, &tmux_domain, 5);
        let target_item = initial_test_pane(5, 71);
        let target_publication = tmux_domain
            .inner
            .prepare_initial_pane_publication(&target_item)
            .expect("prepare revalidation target");
        let target_pane = Arc::clone(target_publication.pane());
        let expected_domain = mux
            .get_domain(tmux_domain.domain_id())
            .expect("exact tmux domain registration");
        tab.commit_unregistered_root_pane(
            &mux,
            &expected_domain,
            tmux_domain.domain_id(),
            &target_pane,
        )
        .expect("publish revalidation target");
        target_publication.commit();
        let mut window = mux.new_empty_window(Some("tmux-target-revalidation".to_string()), None);
        let local_window_id = *window;
        mux.add_tab_to_window(&tab, local_window_id)
            .expect("attach revalidation target tab");
        window.notify();
        let target = mux
            .capture_pane_operation(target_pane.pane_id())
            .expect("capture revalidation target");
        let reservation = tmux_domain
            .inner
            .reserve_test_remote_split(9, 71, 72)
            .expect("reserve revalidation split");
        {
            let mut mirror_index = tmux_domain.inner.mirror_index.lock();
            assert_eq!(
                mirror_index
                    .unregister_pane(71)
                    .expect("remove original target index"),
                Some(target_pane.pane_id())
            );
            mirror_index
                .register_test_pane(target_pane.pane_id(), 79)
                .expect("install changed target mapping");
        }

        let error = tmux_domain
            .inner
            .split_pane(&mux, &target, reservation, SplitRequest::default())
            .expect_err("changed remote target mapping must reject materialization");
        assert!(format!("{error:#}").contains("no longer maps to remote pane 71"));
        assert_eq!(tab.iter_all_panes().len(), 1);
        assert!(!tmux_domain.inner.remote_panes.lock().contains_key(&72));
        assert_eq!(
            tmux_domain
                .inner
                .remote_split_reservations
                .lock()
                .get(&72)
                .expect("rejected split tombstone")
                .load()
                .expect("valid rejected split state"),
            crate::tmux::TmuxRemoteSplitState::Retired
        );
        let queue = tmux_domain.inner.cmd_queue.lock();
        assert_eq!(queue.len(), 1);
        assert_eq!(
            queue
                .front()
                .expect("target-change compensation")
                .get_command(tmux_domain.domain_id()),
            "kill-pane -t %72\n"
        );
        drop(queue);
        let mut mirror_index = tmux_domain.inner.mirror_index.lock();
        assert_eq!(
            mirror_index
                .unregister_pane(79)
                .expect("remove changed target index"),
            Some(target_pane.pane_id())
        );
        mirror_index
            .register_test_pane(target_pane.pane_id(), 71)
            .expect("restore target mapping after negative control");
    }

    #[test]
    fn tmux_atomic_publication_get_config_panic_compensates_and_preserves_progress() {
        assert_split_config_callback_panic_isolated(
            TEST_SPLIT_CONFIG_PANIC_GET,
            8,
            81,
            82,
            83,
            "tmux-get-config-panic",
        );
    }

    #[test]
    fn tmux_atomic_publication_set_config_panic_compensates_and_preserves_progress() {
        assert_split_config_callback_panic_isolated(
            TEST_SPLIT_CONFIG_PANIC_SET,
            9,
            91,
            92,
            93,
            "tmux-set-config-panic",
        );
    }

    #[test]
    fn tmux_atomic_publication_output_lane_full_rolls_back_before_local_cut() {
        assert_split_output_preflight_failure_isolated(
            crate::tmux::TEST_SPLIT_OUTPUT_LANE_FULL,
            "drain_lane_capacity",
            10,
            101,
            102,
            103,
            "tmux-output-lane-full",
        );
    }

    #[test]
    fn tmux_atomic_publication_output_lane_closed_rolls_back_before_local_cut() {
        assert_split_output_preflight_failure_isolated(
            crate::tmux::TEST_SPLIT_OUTPUT_LANE_CLOSED,
            "drain_lane_closed",
            11,
            111,
            112,
            113,
            "tmux-output-lane-closed",
        );
    }

    #[test]
    fn tmux_atomic_publication_output_state_race_rolls_back_before_local_cut() {
        assert_split_output_preflight_failure_isolated(
            crate::tmux::TEST_SPLIT_OUTPUT_STATE_RACE,
            "output gate changed to Ready before local commit",
            12,
            121,
            122,
            123,
            "tmux-output-state-race",
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
            assert!(
                tmux_domain
                    .inner
                    .ingest_mux_notification(MuxNotificationEnvelope {
                        notification: MuxNotification::PaneOutput(77),
                        topology: MuxTopologyStamp::NonTopology,
                    })
            );
        }

        let telemetry = tmux_domain.inner.notification_intent_telemetry.snapshot();
        assert_eq!(telemetry.received, 10_000);
        assert_eq!(telemetry.prefiltered, 10_000);
        assert_eq!(telemetry.scheduled, 0);
        assert_eq!(telemetry.applied, 0);
        assert_eq!(
            tmux_domain.inner.notification_intents.lock().pending_len(),
            0
        );
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
        assert_eq!(
            parse_split_pane_identity("\n  %42  \n"),
            SplitPaneIdentityParse::Exact(42)
        );
        assert!(matches!(
            parse_split_pane_identity("%42\nwarning\n"),
            SplitPaneIdentityParse::RecoverableTrailingOutput { pane_id: 42, .. }
        ));
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
                matches!(
                    parse_split_pane_identity(malformed),
                    SplitPaneIdentityParse::Unresolved(_)
                ),
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
        let cmd = SplitPane::new(0, 5, SplitDirection::Horizontal, 1);
        assert_eq!(
            cmd.get_command(0),
            "split-window -P -F '#{pane_id}' -h -t %5 \\; set-option -p @frankenterm_split_token ft-0-1\n"
        );
    }

    #[test]
    fn split_pane_vertical_get_command() {
        let cmd = SplitPane::new(0, 9, SplitDirection::Vertical, 2);
        assert_eq!(
            cmd.get_command(0),
            "split-window -P -F '#{pane_id}' -v -t %9 \\; set-option -p @frankenterm_split_token ft-0-2\n"
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
        let cmd = KillPane {
            pane_id: 23,
            child_state: Arc::new(TmuxChildState::new()),
        };
        assert_eq!(cmd.get_command(0), "kill-pane -t %23\n");
    }

    #[test]
    fn detach_client_is_a_terminal_command_that_awaits_clean_exit() {
        let cmd = DetachClient;
        assert_eq!(cmd.mailbox_class(), TmuxCommandClass::TerminalControl);
        assert_eq!(cmd.get_command(0), "detach\n");
        assert!(cmd.awaits_clean_exit());
    }

    #[test]
    fn detach_client_preserves_guarded_failure_as_a_typed_error() {
        let err = DetachClient
            .process_result(
                9,
                &Guarded {
                    error: true,
                    timestamp: 0,
                    number: 0,
                    flags: 0,
                    output: "session vanished".to_string(),
                },
            )
            .expect_err("detach-client failure must be terminally distinguishable");
        assert!(err.to_string().contains("detach-client"));
        assert!(err.to_string().contains("domain=9"));
    }

    #[test]
    fn kill_pane_failure_never_reports_matching_remote_success() {
        let child_state = Arc::new(TmuxChildState::new());
        let cmd = KillPane {
            pane_id: 23,
            child_state: Arc::clone(&child_state),
        };
        let err = cmd
            .process_result(
                9,
                &Guarded {
                    error: true,
                    timestamp: 0,
                    number: 0,
                    flags: 0,
                    output: "no such pane".to_string(),
                },
            )
            .expect_err("remote kill failure must remain terminally distinguishable");
        assert!(err.to_string().contains("kill-pane"));
        assert_eq!(
            child_state
                .try_wait()
                .expect("failed remote kill should terminalize the local child")
                .signal(),
            Some("tmux kill-pane failed")
        );
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
    fn active_pane_sync_releases_gui_tabs_before_reentrant_notification() -> anyhow::Result<()> {
        let (_mux_guard, tmux_domain) = install_tmux_domain();
        let mux = tmux_mux()?;
        *tmux_domain.inner.tmux_session.lock() = Some(1);

        let tab = Arc::new(Tab::new(&TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
            dpi: 0,
        }));
        mux.add_tab_no_panes(&tab)?;
        let local_tab_id = tab.tab_id();
        tmux_domain
            .inner
            .mirror_index
            .lock()
            .register_window(local_tab_id, 2)?;
        tmux_domain.inner.gui_tabs.lock().insert(
            2,
            TmuxTab {
                tab_id: local_tab_id,
                tmux_window_id: 2,
                layout_csum: "test".to_string(),
                panes: HashSet::from([3, 4]),
            },
        );

        let pane_item = |pane_id, pane_active| PaneItem {
            session_id: 1,
            window_id: 2,
            pane_id,
            _pane_index: 0,
            cursor_x: 0,
            cursor_y: 0,
            pane_width: 80,
            pane_height: 24,
            pane_left: 0,
            pane_top: 0,
            pane_active,
        };
        let first_item = pane_item(3, false);
        let active_item = pane_item(4, true);
        let (first, first_gate) = tmux_domain
            .inner
            .construct_pane(&first_item, Arc::new(TmuxChildState::new()))?;
        let (active, active_gate) = tmux_domain
            .inner
            .construct_pane(&active_item, Arc::new(TmuxChildState::new()))?;
        insert_test_remote_pane(&tmux_domain, 3, first_gate);
        insert_test_remote_pane(&tmux_domain, 4, active_gate);
        mux.add_pane(&first)?;
        mux.add_pane(&active)?;
        tab.assign_pane(&first);
        tab.split_and_insert(0, SplitRequest::default(), Arc::clone(&active))?;
        tab.set_active_idx(0);

        let callback_observed_unlocked_registry = Arc::new(AtomicBool::new(false));
        let callback_observed = Arc::clone(&callback_observed_unlocked_registry);
        let owner = Arc::downgrade(&tmux_domain.inner);
        let _subscription = mux.subscribe(move |notification| {
            if matches!(notification, MuxNotification::PaneFocused(_)) {
                let available = owner
                    .upgrade()
                    .is_some_and(|owner| owner.gui_tabs.try_lock().is_some());
                callback_observed.store(available, Ordering::Release);
            }
            true
        })?;

        tmux_domain.inner.sync_pane_state(&[active_item])?;
        assert!(
            callback_observed_unlocked_registry.load(Ordering::Acquire),
            "synchronous focus notification must be able to re-enter gui_tabs",
        );
        Ok(())
    }

    #[test]
    fn tmux_atomic_publication_initial_sync_releases_gui_window_for_subscription_fanout()
    -> anyhow::Result<()> {
        let (_mux_guard, tmux_domain) = install_tmux_domain();
        let mux = tmux_mux()?;
        *tmux_domain.inner.tmux_session.lock() = Some(17);
        tmux_domain.inner.subscribe_notification()?;

        let callback_completed = Arc::new(AtomicBool::new(false));
        let callback_observed_unlocked = Arc::new(AtomicBool::new(false));
        let completed = Arc::clone(&callback_completed);
        let observed_unlocked = Arc::clone(&callback_observed_unlocked);
        let owner = Arc::downgrade(&tmux_domain.inner);
        let _subscription = mux.subscribe(move |notification| {
            if matches!(notification, MuxNotification::WindowTopologyChanged(_)) {
                let lock_available = owner
                    .upgrade()
                    .is_some_and(|owner| owner.gui_window.try_lock().is_some());
                observed_unlocked.store(lock_available, Ordering::Release);
                completed.store(true, Ordering::Release);
            }
            true
        })?;

        tmux_domain.inner.sync_window_state(
            &[WindowItem {
                session_id: 17,
                window_id: 171,
                window_width: 80,
                window_height: 24,
                window_active: true,
                window_name: "initial-sync".to_string(),
                layout: Vec::new(),
                layout_csum: "initial-sync".to_string(),
                history_limit: 2_000,
            }],
            false,
        )?;

        assert!(callback_completed.load(Ordering::Acquire));
        assert!(
            callback_observed_unlocked.load(Ordering::Acquire),
            "synchronous initial-window topology fanout must re-enter gui_window without blocking"
        );
        assert!(tmux_domain.inner.notification_sub_id.lock().is_some());
        assert!(!tmux_domain.inner.is_terminal());
        let local_window_id = tmux_domain
            .inner
            .gui_window
            .lock()
            .as_ref()
            .map(|builder| **builder)
            .expect("initial sync retains its GUI window builder");
        assert_eq!(
            mux.get_window(local_window_id)
                .expect("initial sync GUI window")
                .len(),
            1
        );
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
