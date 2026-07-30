use crate::activity::Activity;
use crate::domain::{alloc_domain_id, Domain, DomainId, DomainState};
use crate::localpane::LocalPane;
use crate::pane::{Pane, PaneId};
use crate::tab::{SplitRequest, Tab, TabId};
use crate::tmux_commands::{
    DetachClient, ListAllPanes, ListAllWindows, ListCommands, SplitPane, TmuxCommand,
    TmuxCommandClass,
};
use crate::tmux_pty::TmuxChildState;
use crate::window::WindowId;
use crate::{
    Mux, MuxWindowBuilder, PaneOperationGuard, SplitCommitReceipt, TopologyRevision,
};
use anyhow::Context;
use async_trait::async_trait;
use config::configuration;
use crossbeam::channel::{bounded, Receiver, Sender, TrySendError};
use filedescriptor::FileDescriptor;
use frankenterm_term::TerminalSize;
use lru::LruCache;
use parking_lot::Mutex;
use portable_pty::CommandBuilder;
use std::cell::Cell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::convert::TryFrom;
use std::fmt;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};
use termwiz::tmux_cc::*;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct TmuxBacklogLimits {
    per_pane_bytes: usize,
    total_bytes: usize,
    entries: usize,
}

impl TmuxBacklogLimits {
    pub(crate) fn current() -> Self {
        let config = configuration();
        Self {
            per_pane_bytes: config.mux_tmux_max_backlog_bytes_per_pane,
            total_bytes: config.mux_tmux_max_backlog_bytes,
            entries: config.mux_tmux_max_backlog_entries,
        }
    }

    #[cfg(test)]
    pub(crate) fn new(per_pane_bytes: usize, total_bytes: usize, entries: usize) -> Self {
        Self {
            per_pane_bytes,
            total_bytes,
            entries,
        }
    }
}

#[derive(Debug, Default)]
struct PaneBacklog {
    bytes: VecDeque<u8>,
    gapped: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TmuxBacklogDrain {
    Bytes(VecDeque<u8>),
    ResyncRequired,
}

#[derive(Debug)]
pub(crate) struct TmuxBacklog {
    entries: LruCache<TmuxPaneId, PaneBacklog>,
    total_bytes: usize,
    limits: TmuxBacklogLimits,
    resync_all: bool,
    dropped_bytes: u64,
    evicted_entries: u64,
    reported_dropped_bytes: u64,
    reported_evicted_entries: u64,
}

impl Default for TmuxBacklog {
    fn default() -> Self {
        Self {
            entries: LruCache::unbounded(),
            total_bytes: 0,
            limits: TmuxBacklogLimits::default(),
            resync_all: false,
            dropped_bytes: 0,
            evicted_entries: 0,
            reported_dropped_bytes: 0,
            reported_evicted_entries: 0,
        }
    }
}

impl TmuxBacklog {
    pub(crate) fn append_with_limits(
        &mut self,
        pane_id: TmuxPaneId,
        payload: &[u8],
        limits: TmuxBacklogLimits,
    ) {
        self.refresh_limits(limits);
        if payload.is_empty() {
            self.record_metrics();
            return;
        }
        if limits.per_pane_bytes == 0 || limits.total_bytes == 0 || limits.entries == 0 {
            self.dropped_bytes = self
                .dropped_bytes
                .saturating_add(u64::try_from(payload.len()).unwrap_or(u64::MAX));
            self.mark_global_resync();
            self.record_metrics();
            return;
        }
        if self.resync_all {
            self.dropped_bytes = self
                .dropped_bytes
                .saturating_add(u64::try_from(payload.len()).unwrap_or(u64::MAX));
            self.record_metrics();
            return;
        }

        let pane_cap = limits.per_pane_bytes.min(limits.total_bytes);
        let mut entry = self.entries.pop(&pane_id).unwrap_or_default();
        let old_len = entry.bytes.len();
        self.total_bytes = self.total_bytes.saturating_sub(old_len);

        if entry.gapped {
            self.dropped_bytes = self
                .dropped_bytes
                .saturating_add(u64::try_from(payload.len()).unwrap_or(u64::MAX));
            entry.bytes = VecDeque::new();
        } else if old_len.saturating_add(payload.len()) > pane_cap {
            // Never retain an arbitrary terminal-stream suffix. It could begin
            // inside UTF-8, CSI, OSC, or another stateful escape sequence.
            // Preserve a bounded marker and require an authoritative capture.
            let dropped = old_len.saturating_add(payload.len());
            self.dropped_bytes = self
                .dropped_bytes
                .saturating_add(u64::try_from(dropped).unwrap_or(u64::MAX));
            entry.bytes = VecDeque::new();
            entry.gapped = true;
        } else {
            entry.bytes.extend(payload.iter().copied());
        }
        self.total_bytes = self.total_bytes.saturating_add(entry.bytes.len());
        self.entries.put(pane_id, entry);

        self.enforce_entry_and_aggregate_limits();
        self.record_metrics();
    }

    pub(crate) fn refresh_limits(&mut self, limits: TmuxBacklogLimits) {
        if limits == self.limits {
            return;
        }
        self.limits = limits;
        if limits.per_pane_bytes == 0 || limits.total_bytes == 0 || limits.entries == 0 {
            self.mark_global_resync();
            self.record_metrics();
            return;
        }
        if self.resync_all {
            self.record_metrics();
            return;
        }

        let pane_cap = limits.per_pane_bytes.min(limits.total_bytes);
        let pane_ids = self
            .entries
            .iter()
            .map(|(pane_id, _)| *pane_id)
            .collect::<Vec<_>>();
        for pane_id in pane_ids {
            let Some(entry) = self.entries.peek_mut(&pane_id) else {
                continue;
            };
            if entry.bytes.len() > pane_cap {
                let dropped = entry.bytes.len();
                entry.bytes = VecDeque::new();
                entry.gapped = true;
                self.total_bytes = self.total_bytes.saturating_sub(dropped);
                self.dropped_bytes = self
                    .dropped_bytes
                    .saturating_add(u64::try_from(dropped).unwrap_or(u64::MAX));
            }
            if entry.bytes.capacity() > pane_cap.saturating_mul(2) {
                entry.bytes.shrink_to_fit();
            }
        }
        self.enforce_entry_and_aggregate_limits();
    }

    fn enforce_entry_and_aggregate_limits(&mut self) {
        let overflow_entry = if self.entries.len() > self.limits.entries {
            self.entries.pop_lru()
        } else {
            None
        };
        if let Some((_pane_id, entry)) = overflow_entry {
            self.total_bytes = self.total_bytes.saturating_sub(entry.bytes.len());
            self.dropped_bytes = self
                .dropped_bytes
                .saturating_add(u64::try_from(entry.bytes.len()).unwrap_or(u64::MAX));
            self.evicted_entries = self.evicted_entries.saturating_add(1);
            // Losing the marker itself means we no longer know which pane is
            // incomplete. The only fail-closed bounded representation is one
            // global resynchronization requirement.
            self.mark_global_resync();
            return;
        }

        while self.total_bytes > self.limits.total_bytes {
            let Some(pane_id) = self
                .entries
                .iter()
                .filter(|(_, entry)| !entry.bytes.is_empty())
                .map(|(pane_id, _)| *pane_id)
                .next_back()
            else {
                self.total_bytes = 0;
                break;
            };
            let entry = self
                .entries
                .peek_mut(&pane_id)
                .expect("selected tmux backlog entry disappeared");
            let dropped = entry.bytes.len();
            entry.bytes = VecDeque::new();
            entry.gapped = true;
            self.total_bytes = self.total_bytes.saturating_sub(dropped);
            self.dropped_bytes = self
                .dropped_bytes
                .saturating_add(u64::try_from(dropped).unwrap_or(u64::MAX));
        }
    }

    pub(crate) fn take(&mut self, pane_id: TmuxPaneId) -> Option<TmuxBacklogDrain> {
        let entry = self.entries.pop(&pane_id)?;
        self.total_bytes = self.total_bytes.saturating_sub(entry.bytes.len());
        self.record_metrics();
        if entry.gapped {
            Some(TmuxBacklogDrain::ResyncRequired)
        } else {
            Some(TmuxBacklogDrain::Bytes(entry.bytes))
        }
    }

    #[cfg(test)]
    pub(crate) fn take_global_resync(&mut self) -> bool {
        let resync = std::mem::take(&mut self.resync_all);
        self.record_metrics();
        resync
    }

    pub(crate) fn requires_global_resync(&self) -> bool {
        self.resync_all
    }

    pub(crate) fn requires_recovery(&self) -> bool {
        self.resync_all || self.entries.iter().any(|(_, entry)| entry.gapped)
    }

    pub(crate) fn remove(&mut self, pane_id: TmuxPaneId) -> bool {
        let Some(entry) = self.entries.pop(&pane_id) else {
            return false;
        };
        self.total_bytes = self.total_bytes.saturating_sub(entry.bytes.len());
        self.record_metrics();
        true
    }

    pub(crate) fn remove_many(&mut self, pane_ids: &[TmuxPaneId]) {
        for pane_id in pane_ids {
            let _ = self.remove(*pane_id);
        }
    }

    pub(crate) fn clear(&mut self) {
        self.resync_all = false;
        self.clear_entries();
        self.record_metrics();
    }

    fn mark_global_resync(&mut self) {
        self.resync_all = true;
        self.clear_entries();
    }

    fn clear_entries(&mut self) {
        self.dropped_bytes = self
            .dropped_bytes
            .saturating_add(u64::try_from(self.total_bytes).unwrap_or(u64::MAX));
        self.evicted_entries = self
            .evicted_entries
            .saturating_add(u64::try_from(self.entries.len()).unwrap_or(u64::MAX));
        // LruCache::clear, like VecDeque::clear, retains the backing allocation.
        // Replacing the cache is required for the logical byte cap to also
        // bound resident memory after a gap or limit contraction.
        self.entries = LruCache::unbounded();
        self.total_bytes = 0;
    }

    fn record_metrics(&mut self) {
        let newly_dropped = self
            .dropped_bytes
            .saturating_sub(self.reported_dropped_bytes);
        if newly_dropped > 0 {
            metrics::counter!("mux.tmux.backlog.dropped_bytes").increment(newly_dropped);
            self.reported_dropped_bytes = self.dropped_bytes;
        }
        let newly_evicted = self
            .evicted_entries
            .saturating_sub(self.reported_evicted_entries);
        if newly_evicted > 0 {
            metrics::counter!("mux.tmux.backlog.evicted_entries").increment(newly_evicted);
            self.reported_evicted_entries = self.evicted_entries;
        }
        metrics::histogram!("mux.tmux.backlog.entries").record(self.entries.len() as f64);
        metrics::histogram!("mux.tmux.backlog.bytes").record(self.total_bytes as f64);
    }

    #[cfg(test)]
    pub(crate) fn contains(&self, pane_id: TmuxPaneId) -> bool {
        self.entries.contains(&pane_id)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    #[cfg(test)]
    fn pane_bytes(&self, pane_id: TmuxPaneId) -> Option<Vec<u8>> {
        self.entries
            .peek(&pane_id)
            .map(|entry| entry.bytes.iter().copied().collect())
    }

    #[cfg(test)]
    fn pane_requires_resync(&self, pane_id: TmuxPaneId) -> bool {
        self.entries
            .peek(&pane_id)
            .is_some_and(|entry| entry.gapped)
    }

    #[cfg(test)]
    fn retained_byte_capacity(&self) -> usize {
        self.entries
            .iter()
            .map(|(_, entry)| entry.bytes.capacity())
            .sum()
    }
}

/// Warning threshold for tmux command queue depth. Exceeding this
/// indicates protocol churn or a stalled consumer.
const CMD_QUEUE_WARNING_DEPTH: usize = 10_000;

/// Hard cap on tmux command queue size to prevent unbounded memory growth
/// during event storms. Producers receive an explicit rejection at the cap;
/// already acknowledged commands are never displaced.
pub(crate) const CMD_QUEUE_MAX_DEPTH: usize = 50_000;

/// Aggregate retained `SendKeys` payload across queued, preparing, and
/// in-flight commands. The count cap alone cannot bound a single paste.
pub(crate) const CMD_QUEUE_MAX_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;

/// Non-control work may consume at most this many retained command slots.
/// The remainder is progress authority reserved for required and terminal
/// control, even when key input and resize/focus storms arrive concurrently.
const CMD_QUEUE_CONTROL_RESERVED_SLOTS: usize = 4_096;

/// Required non-terminal control may not consume these final slots. A remote
/// pane can therefore always admit bounded terminal control after saturation.
const CMD_QUEUE_TERMINAL_RESERVED_SLOTS: usize = 64;

/// Coalescible intents have their own bounded lane. This is deliberately
/// smaller than the aggregate queue and independent of lossless input.
const CMD_QUEUE_INTENT_MAX_DEPTH: usize = 4_096;

/// Preserve FIFO order within the durable lane while guaranteeing that a
/// retained latest-intent resize/focus command is serviced after at most this
/// many additional durable commands.
const CMD_QUEUE_DURABLE_SERVICE_QUANTUM: usize = 32;

/// A domain can retain at most one command/detach start, one matching
/// completion signal, and one terminal signal while its I/O supervisor is
/// between receive sites.
const TMUX_IO_CONTROL_CAPACITY: usize = 3;

/// The dedicated writer accepts only the supervisor's single current job.
const TMUX_IO_WRITE_CAPACITY: usize = 1;

/// A write publishes one started edge and one terminal result.
const TMUX_IO_RESULT_CAPACITY: usize = 2;

/// Events received after a guarded command response must wait for that
/// response's main-thread mutation to commit. Bound the barrier so a stalled
/// main thread cannot turn the parser into an unbounded retention path.
const PROTOCOL_BARRIER_MAX_EVENTS: usize = 65_536;
const PROTOCOL_BARRIER_MAX_BYTES: usize = 16 * 1024 * 1024;
const PROTOCOL_BARRIER_DRAIN_EVENT_QUANTUM: usize = 256;
const PROTOCOL_BARRIER_DRAIN_BYTE_QUANTUM: usize = 512 * 1024;

/// Bound one main-thread notification-intent runnable so a continuously
/// mutating UI cannot monopolize the event loop. Each drain step consumes at
/// most two latest-wins intents, so keep this an even number.
pub(crate) const NOTIFICATION_INTENT_DRAIN_QUANTUM: usize = 32;

/// Post-fence topology callbacks can be delivered out of revision order by
/// independent internal queues. Retain a bounded gap window so a later
/// focus/window event never overtakes an earlier one. Exceeding the window is
/// a fail-closed ordering failure, not permission to guess.
pub(crate) const NOTIFICATION_INTENT_MAX_REORDER_GAP: usize = 4_096;

/// Tmux pane identifiers are unique for the lifetime of a server. Retaining a
/// bounded tombstone prevents late output for a detached pane from being
/// mistaken for pre-attach output for a future pane.
pub(crate) const RETIRED_PANE_TOMBSTONE_LIMIT: usize = 65_536;

// The closeable command mailbox owns a distinct externally in-flight request,
// allowing cap enforcement and tail coalescing to preserve the exact command
// whose Guarded response is still pending.

#[derive(PartialEq, Eq, Debug, Copy, Clone)]
pub enum AttachState {
    Init,
    Done,
}

#[derive(PartialEq, Eq, Debug, Copy, Clone)]
enum State {
    WaitForInitialGuard,
    Idle,
    /// A sender owns the right to prepare the queue head but has not yet made
    /// the command externally visible.
    Sending,
    WaitingForResponse,
    /// The guarded response has arrived, but its main-thread state mutation
    /// has not committed yet. No later command may be sent in this state.
    ProcessingResponse,
    Exit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TmuxPaneOutputState {
    /// A newly created empty local terminal may accept its complete bounded
    /// pre-publication stream.
    Fresh,
    /// An existing remote terminal is waiting for its queued capture result.
    AwaitingCapture,
    /// The capture committed. Any output that raced after it remains in the
    /// backlog and makes textual recovery unsafe.
    Captured,
    /// The initial stream/capture and cursor transaction committed; live
    /// output may now write directly.
    Ready,
    /// The remote pane was detached. Any producer that retained the pane gate
    /// must discard late output rather than resurrecting a backlog entry.
    Retired,
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
    pub output_state: TmuxPaneOutputState,
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

/// Bidirectional identity indexes for the local tmux mirror.
///
/// Notification callbacks start with local mux identifiers while tmux
/// commands require remote identifiers. Keeping the reverse edges beside the
/// authoritative remote-keyed maps turns focus/resize storms from repeated
/// O(panes + tabs) scans into bounded O(1) lookups. Callers mutate an
/// authoritative map and this index under the map -> index lock order.
#[derive(Debug, Default)]
pub(crate) struct TmuxMirrorIndex {
    pane_by_local: HashMap<PaneId, TmuxPaneId>,
    pane_by_remote: HashMap<TmuxPaneId, PaneId>,
    window_by_local_tab: HashMap<TabId, TmuxWindowId>,
    tab_by_remote_window: HashMap<TmuxWindowId, TabId>,
}

impl TmuxMirrorIndex {
    pub(crate) fn register_pane(
        &mut self,
        local_pane_id: PaneId,
        remote_pane_id: TmuxPaneId,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.pane_by_local.contains_key(&local_pane_id),
            "local pane {local_pane_id} already has a tmux reverse-index entry"
        );
        anyhow::ensure!(
            !self.pane_by_remote.contains_key(&remote_pane_id),
            "remote tmux pane {remote_pane_id} already has a local reverse-index entry"
        );
        self.pane_by_local.insert(local_pane_id, remote_pane_id);
        self.pane_by_remote.insert(remote_pane_id, local_pane_id);
        Ok(())
    }

    pub(crate) fn unregister_pane(
        &mut self,
        remote_pane_id: TmuxPaneId,
    ) -> anyhow::Result<Option<PaneId>> {
        let Some(local_pane_id) = self.pane_by_remote.remove(&remote_pane_id) else {
            anyhow::ensure!(
                !self
                    .pane_by_local
                    .values()
                    .any(|candidate| *candidate == remote_pane_id),
                "remote tmux pane {remote_pane_id} has a one-sided local reverse-index entry"
            );
            return Ok(None);
        };
        anyhow::ensure!(
            self.pane_by_local.remove(&local_pane_id) == Some(remote_pane_id),
            "tmux pane reverse index disagrees for local pane {local_pane_id} and remote pane \
             {remote_pane_id}"
        );
        Ok(Some(local_pane_id))
    }

    pub(crate) fn register_window(
        &mut self,
        local_tab_id: TabId,
        remote_window_id: TmuxWindowId,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.window_by_local_tab.contains_key(&local_tab_id),
            "local tab {local_tab_id} already has a tmux reverse-index entry"
        );
        anyhow::ensure!(
            !self.tab_by_remote_window.contains_key(&remote_window_id),
            "remote tmux window {remote_window_id} already has a local reverse-index entry"
        );
        self.window_by_local_tab
            .insert(local_tab_id, remote_window_id);
        self.tab_by_remote_window
            .insert(remote_window_id, local_tab_id);
        Ok(())
    }

    pub(crate) fn unregister_window(
        &mut self,
        remote_window_id: TmuxWindowId,
    ) -> anyhow::Result<Option<TabId>> {
        let Some(local_tab_id) = self.tab_by_remote_window.remove(&remote_window_id) else {
            anyhow::ensure!(
                !self
                    .window_by_local_tab
                    .values()
                    .any(|candidate| *candidate == remote_window_id),
                "remote tmux window {remote_window_id} has a one-sided local reverse-index entry"
            );
            return Ok(None);
        };
        anyhow::ensure!(
            self.window_by_local_tab.remove(&local_tab_id) == Some(remote_window_id),
            "tmux window reverse index disagrees for local tab {local_tab_id} and remote window \
             {remote_window_id}"
        );
        Ok(Some(local_tab_id))
    }

    pub(crate) fn remote_pane_for_local(&self, local_pane_id: PaneId) -> Option<TmuxPaneId> {
        self.pane_by_local.get(&local_pane_id).copied()
    }

    pub(crate) fn remote_window_for_local_tab(
        &self,
        local_tab_id: TabId,
    ) -> Option<TmuxWindowId> {
        self.window_by_local_tab.get(&local_tab_id).copied()
    }

    pub(crate) fn clear(&mut self) {
        self.pane_by_local.clear();
        self.pane_by_remote.clear();
        self.window_by_local_tab.clear();
        self.tab_by_remote_window.clear();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TmuxNotificationIntent {
    PaneFocused(PaneId),
    WindowInvalidated(WindowId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SequencedTmuxNotificationIntent {
    pub(crate) revision: TopologyRevision,
    pub(crate) intent: TmuxNotificationIntent,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct TmuxNotificationIntentOffer {
    pub(crate) schedule: bool,
    pub(crate) coalesced: bool,
    pub(crate) closed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TmuxNotificationIntentRunDisposition {
    Idle,
    Reschedule,
    WaitingForCapacity,
    Closed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TmuxTopologyBarrierEvent {
    Barrier,
    Intent(TmuxNotificationIntent),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct TmuxTopologyObservation {
    pub(crate) schedule: bool,
    pub(crate) coalesced: u64,
    pub(crate) stale: bool,
    pub(crate) closed: bool,
}

/// At most one latest pane-focus and one latest window-invalidation intent are
/// retained per tmux domain. Authoritative mux topology revisions provide the
/// cross-kind ordering barrier and reject delayed callbacks that would
/// otherwise regress the final remote selection.
#[derive(Debug, Default)]
pub(crate) struct TmuxNotificationIntentState {
    pending_pane_focus: Option<SequencedTmuxNotificationIntent>,
    pending_window_invalidation: Option<SequencedTmuxNotificationIntent>,
    latest_pane_focus_revision: Option<TopologyRevision>,
    latest_window_invalidation_revision: Option<TopologyRevision>,
    next_topology_revision: Option<TopologyRevision>,
    topology_reorder: VecDeque<Option<TmuxTopologyBarrierEvent>>,
    runnable_scheduled: bool,
    waiting_for_capacity: bool,
    closed: bool,
}

impl TmuxNotificationIntentState {
    pub(crate) fn initialize_topology_order(
        &mut self,
        baseline_revision: TopologyRevision,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.closed,
            "tmux notification topology coordinator is closed"
        );
        anyhow::ensure!(
            self.next_topology_revision.is_none() && self.topology_reorder.is_empty(),
            "tmux notification topology coordinator was initialized more than once"
        );
        let next = baseline_revision
            .get()
            .checked_add(1)
            .map(TopologyRevision::new)
            .context("tmux notification topology baseline is exhausted")?;
        self.next_topology_revision = Some(next);
        Ok(())
    }

    pub(crate) fn observe_topology_event(
        &mut self,
        revision: TopologyRevision,
        event: TmuxTopologyBarrierEvent,
    ) -> anyhow::Result<TmuxTopologyObservation> {
        if self.closed {
            return Ok(TmuxTopologyObservation {
                closed: true,
                ..TmuxTopologyObservation::default()
            });
        }
        let next_revision = self
            .next_topology_revision
            .context("tmux notification topology coordinator is not initialized")?;
        if revision < next_revision {
            return Ok(TmuxTopologyObservation {
                stale: true,
                ..TmuxTopologyObservation::default()
            });
        }

        let offset_u64 = revision
            .get()
            .checked_sub(next_revision.get())
            .context("tmux notification topology revision subtraction underflow")?;
        let offset = usize::try_from(offset_u64)
            .context("tmux notification topology gap does not fit usize")?;
        anyhow::ensure!(
            offset < NOTIFICATION_INTENT_MAX_REORDER_GAP,
            "tmux notification topology gap {offset} exceeds bounded window {}",
            NOTIFICATION_INTENT_MAX_REORDER_GAP
        );
        if self.topology_reorder.len() <= offset {
            self.topology_reorder.resize(offset.saturating_add(1), None);
        }
        anyhow::ensure!(
            self.topology_reorder[offset].is_none(),
            "duplicate tmux notification topology revision {}",
            revision.get()
        );
        self.topology_reorder[offset] = Some(event);
        if offset > 0 {
            metrics::counter!("mux.tmux.notification_intent.reordered").increment(1);
        }
        metrics::histogram!("mux.tmux.notification_intent.reorder_buffer_depth")
            .record(self.topology_reorder.len() as f64);

        let mut observation = TmuxTopologyObservation::default();
        while let Some(Some(event)) = self.topology_reorder.front().copied() {
            let current_revision = self
                .next_topology_revision
                .context("tmux notification topology revision exhausted with retained events")?;
            let _ = self.topology_reorder.pop_front();
            self.next_topology_revision = current_revision
                .get()
                .checked_add(1)
                .map(TopologyRevision::new);

            if let TmuxTopologyBarrierEvent::Intent(intent) = event {
                let offer = self.offer(SequencedTmuxNotificationIntent {
                    revision: current_revision,
                    intent,
                });
                observation.schedule |= offer.schedule;
                observation.coalesced = observation
                    .coalesced
                    .saturating_add(u64::from(offer.coalesced));
                observation.closed |= offer.closed;
            }
        }
        anyhow::ensure!(
            self.next_topology_revision.is_some() || self.topology_reorder.is_empty(),
            "tmux notification topology revision exhausted before buffered events drained"
        );
        Ok(observation)
    }

    fn slot_mut(
        &mut self,
        intent: TmuxNotificationIntent,
    ) -> (
        &mut Option<SequencedTmuxNotificationIntent>,
        &mut Option<TopologyRevision>,
    ) {
        match intent {
            TmuxNotificationIntent::PaneFocused(_) => (
                &mut self.pending_pane_focus,
                &mut self.latest_pane_focus_revision,
            ),
            TmuxNotificationIntent::WindowInvalidated(_) => (
                &mut self.pending_window_invalidation,
                &mut self.latest_window_invalidation_revision,
            ),
        }
    }

    pub(crate) fn offer(
        &mut self,
        sequenced: SequencedTmuxNotificationIntent,
    ) -> TmuxNotificationIntentOffer {
        if self.closed {
            return TmuxNotificationIntentOffer {
                closed: true,
                ..TmuxNotificationIntentOffer::default()
            };
        }

        let (slot, latest_revision) = self.slot_mut(sequenced.intent);
        if latest_revision.is_some_and(|latest| latest >= sequenced.revision) {
            return TmuxNotificationIntentOffer {
                coalesced: true,
                ..TmuxNotificationIntentOffer::default()
            };
        }

        *latest_revision = Some(sequenced.revision);
        let coalesced = slot.replace(sequenced).is_some();
        let schedule = !self.runnable_scheduled && !self.waiting_for_capacity;
        if schedule {
            self.runnable_scheduled = true;
        }
        TmuxNotificationIntentOffer {
            schedule,
            coalesced,
            closed: false,
        }
    }

    pub(crate) fn take_ordered_batch(
        &mut self,
    ) -> [Option<SequencedTmuxNotificationIntent>; 2] {
        debug_assert!(self.runnable_scheduled);
        debug_assert!(!self.waiting_for_capacity);
        let mut batch = [
            self.pending_pane_focus.take(),
            self.pending_window_invalidation.take(),
        ];
        if matches!(
            batch,
            [Some(first), Some(second)] if first.revision > second.revision
        ) {
            batch.swap(0, 1);
        }
        batch
    }

    pub(crate) fn is_current(&self, sequenced: SequencedTmuxNotificationIntent) -> bool {
        match sequenced.intent {
            TmuxNotificationIntent::PaneFocused(_) => {
                self.latest_pane_focus_revision == Some(sequenced.revision)
            }
            TmuxNotificationIntent::WindowInvalidated(_) => {
                self.latest_window_invalidation_revision == Some(sequenced.revision)
            }
        }
    }

    fn restore_if_current(&mut self, sequenced: SequencedTmuxNotificationIntent) -> bool {
        if !self.is_current(sequenced) {
            return false;
        }
        let (slot, _) = self.slot_mut(sequenced.intent);
        if slot.is_none() {
            *slot = Some(sequenced);
        }
        true
    }

    /// Re-publish an unadmitted batch while the command mailbox mutex is still
    /// held. A consumer cannot free capacity before the waiting edge becomes
    /// visible, closing the otherwise subtle full->free lost-wakeup race.
    pub(crate) fn wait_for_capacity(
        &mut self,
        failed: SequencedTmuxNotificationIntent,
        remaining: Option<SequencedTmuxNotificationIntent>,
    ) -> u64 {
        if self.closed {
            self.runnable_scheduled = false;
            self.waiting_for_capacity = false;
            return 1u64.saturating_add(u64::from(remaining.is_some()));
        }
        let mut superseded = u64::from(!self.restore_if_current(failed));
        if let Some(remaining) = remaining {
            superseded =
                superseded.saturating_add(u64::from(!self.restore_if_current(remaining)));
        }
        self.runnable_scheduled = false;
        self.waiting_for_capacity =
            self.pending_pane_focus.is_some() || self.pending_window_invalidation.is_some();
        superseded
    }

    pub(crate) fn capacity_available(&mut self) -> bool {
        if self.closed || !self.waiting_for_capacity {
            return false;
        }
        self.waiting_for_capacity = false;
        if self.pending_pane_focus.is_none() && self.pending_window_invalidation.is_none() {
            return false;
        }
        debug_assert!(!self.runnable_scheduled);
        self.runnable_scheduled = true;
        true
    }

    pub(crate) fn finish_quantum(&mut self) -> TmuxNotificationIntentRunDisposition {
        if self.closed {
            self.runnable_scheduled = false;
            self.waiting_for_capacity = false;
            return TmuxNotificationIntentRunDisposition::Closed;
        }
        if self.waiting_for_capacity {
            self.runnable_scheduled = false;
            return TmuxNotificationIntentRunDisposition::WaitingForCapacity;
        }
        if self.pending_pane_focus.is_some() || self.pending_window_invalidation.is_some() {
            debug_assert!(self.runnable_scheduled);
            TmuxNotificationIntentRunDisposition::Reschedule
        } else {
            self.runnable_scheduled = false;
            TmuxNotificationIntentRunDisposition::Idle
        }
    }

    pub(crate) fn cancel_scheduled_runnable(&mut self) -> bool {
        self.runnable_scheduled = false;
        !self.closed
            && (self.pending_pane_focus.is_some()
                || self.pending_window_invalidation.is_some())
    }

    pub(crate) fn close(&mut self) {
        self.pending_pane_focus = None;
        self.pending_window_invalidation = None;
        self.latest_pane_focus_revision = None;
        self.latest_window_invalidation_revision = None;
        self.next_topology_revision = None;
        self.topology_reorder.clear();
        self.runnable_scheduled = false;
        self.waiting_for_capacity = false;
        self.closed = true;
    }

    #[cfg(test)]
    pub(crate) fn pending_len(&self) -> usize {
        usize::from(self.pending_pane_focus.is_some())
            + usize::from(self.pending_window_invalidation.is_some())
    }

    #[cfg(test)]
    pub(crate) fn is_waiting_for_capacity(&self) -> bool {
        self.waiting_for_capacity
    }
}

pub(crate) struct TmuxNotificationIntentTelemetry {
    received: metrics::Counter,
    prefiltered: metrics::Counter,
    coalesced: metrics::Counter,
    scheduled: metrics::Counter,
    applied: metrics::Counter,
    dropped_stale: metrics::Counter,
    backpressured: metrics::Counter,
    #[cfg(test)]
    test_received: AtomicU64,
    #[cfg(test)]
    test_prefiltered: AtomicU64,
    #[cfg(test)]
    test_coalesced: AtomicU64,
    #[cfg(test)]
    test_scheduled: AtomicU64,
    #[cfg(test)]
    test_applied: AtomicU64,
    #[cfg(test)]
    test_dropped_stale: AtomicU64,
    #[cfg(test)]
    test_backpressured: AtomicU64,
}

impl Default for TmuxNotificationIntentTelemetry {
    fn default() -> Self {
        Self {
            // Register once per domain. The storm path then increments cached
            // handles instead of resolving seven metric keys per callback.
            received: metrics::counter!("mux.tmux.notification_intent.received"),
            prefiltered: metrics::counter!("mux.tmux.notification_intent.prefiltered"),
            coalesced: metrics::counter!("mux.tmux.notification_intent.coalesced"),
            scheduled: metrics::counter!("mux.tmux.notification_intent.scheduled"),
            applied: metrics::counter!("mux.tmux.notification_intent.applied"),
            dropped_stale: metrics::counter!("mux.tmux.notification_intent.dropped_stale"),
            backpressured: metrics::counter!("mux.tmux.notification_intent.backpressured"),
            #[cfg(test)]
            test_received: AtomicU64::new(0),
            #[cfg(test)]
            test_prefiltered: AtomicU64::new(0),
            #[cfg(test)]
            test_coalesced: AtomicU64::new(0),
            #[cfg(test)]
            test_scheduled: AtomicU64::new(0),
            #[cfg(test)]
            test_applied: AtomicU64::new(0),
            #[cfg(test)]
            test_dropped_stale: AtomicU64::new(0),
            #[cfg(test)]
            test_backpressured: AtomicU64::new(0),
        }
    }
}

impl TmuxNotificationIntentTelemetry {
    pub(crate) fn record_received(&self) {
        #[cfg(test)]
        self.test_received.fetch_add(1, Ordering::Relaxed);
        self.received.increment(1);
    }

    pub(crate) fn record_prefiltered(&self) {
        #[cfg(test)]
        self.test_prefiltered.fetch_add(1, Ordering::Relaxed);
        self.prefiltered.increment(1);
    }

    pub(crate) fn record_coalesced(&self, count: u64) {
        if count == 0 {
            return;
        }
        #[cfg(test)]
        self.test_coalesced.fetch_add(count, Ordering::Relaxed);
        self.coalesced.increment(count);
    }

    pub(crate) fn record_scheduled(&self) {
        #[cfg(test)]
        self.test_scheduled.fetch_add(1, Ordering::Relaxed);
        self.scheduled.increment(1);
    }

    pub(crate) fn record_applied(&self) {
        #[cfg(test)]
        self.test_applied.fetch_add(1, Ordering::Relaxed);
        self.applied.increment(1);
    }

    pub(crate) fn record_dropped_stale(&self) {
        #[cfg(test)]
        self.test_dropped_stale.fetch_add(1, Ordering::Relaxed);
        self.dropped_stale.increment(1);
    }

    pub(crate) fn record_backpressured(&self) {
        #[cfg(test)]
        self.test_backpressured.fetch_add(1, Ordering::Relaxed);
        self.backpressured.increment(1);
    }

    #[cfg(test)]
    pub(crate) fn snapshot(&self) -> TmuxNotificationIntentTelemetrySnapshot {
        TmuxNotificationIntentTelemetrySnapshot {
            received: self.test_received.load(Ordering::Relaxed),
            prefiltered: self.test_prefiltered.load(Ordering::Relaxed),
            coalesced: self.test_coalesced.load(Ordering::Relaxed),
            scheduled: self.test_scheduled.load(Ordering::Relaxed),
            applied: self.test_applied.load(Ordering::Relaxed),
            dropped_stale: self.test_dropped_stale.load(Ordering::Relaxed),
            backpressured: self.test_backpressured.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct TmuxNotificationIntentTelemetrySnapshot {
    pub(crate) received: u64,
    pub(crate) prefiltered: u64,
    pub(crate) coalesced: u64,
    pub(crate) scheduled: u64,
    pub(crate) applied: u64,
    pub(crate) dropped_stale: u64,
    pub(crate) backpressured: u64,
}

#[derive(Debug)]
pub(crate) struct TmuxCmdQueue {
    durable_entries: VecDeque<Box<dyn TmuxCommand>>,
    intent_entries: VecDeque<Box<dyn TmuxCommand>>,
    in_flight: Option<InFlightTmuxCommand>,
    preparing: Option<PreparingTmuxCommand>,
    retained_by_class: [usize; TmuxCommandClass::COUNT],
    durable_since_intent: usize,
    payload_bytes: usize,
    closed: bool,
    terminal_barrier: bool,
    terminal_command_dispatched: bool,
    rejected_commands: u64,
    merged_commands: u64,
}

#[derive(Debug)]
struct PreparingTmuxCommand {
    class: TmuxCommandClass,
    payload_bytes: usize,
}

#[derive(Debug, Default)]
struct TmuxProtocolBarrier {
    active: bool,
    events: VecDeque<Event>,
    retained_bytes: usize,
    response_retained_bytes: usize,
    rejected_batches: u64,
}

impl TmuxProtocolBarrier {
    fn event_retained_bytes(event: &Event) -> usize {
        let dynamic = match event {
            Event::Guarded(response) => response.output.capacity(),
            Event::ClientDetached { client_name } => client_name.capacity(),
            Event::ClientSessionChanged {
                client_name,
                session_name,
                ..
            } => client_name
                .capacity()
                .saturating_add(session_name.capacity()),
            Event::ConfigError { error } => error.capacity(),
            Event::ExtendedOutput { text, .. } | Event::Output { text, .. } => text.capacity(),
            Event::Exit { reason } => reason.as_ref().map_or(0, String::capacity),
            Event::LayoutChange {
                layout,
                visible_layout,
                raw_flags,
                ..
            } => layout
                .capacity()
                .saturating_add(visible_layout.as_ref().map_or(0, String::capacity))
                .saturating_add(raw_flags.as_ref().map_or(0, String::capacity)),
            Event::Message { message } => message.capacity(),
            Event::PasteBufferChanged { buffer } | Event::PasteBufferDeleted { buffer } => {
                buffer.capacity()
            }
            Event::SessionChanged { name, .. }
            | Event::SessionRenamed { name }
            | Event::WindowRenamed { name, .. } => name.capacity(),
            _ => 0,
        };
        std::mem::size_of::<Event>().saturating_add(dynamic)
    }

    fn activate(&mut self, response_retained_bytes: usize, events: Vec<Event>) -> Result<(), ()> {
        debug_assert!(!self.active);
        self.active = true;
        self.response_retained_bytes = response_retained_bytes;
        self.retained_bytes = response_retained_bytes;
        if response_retained_bytes > PROTOCOL_BARRIER_MAX_BYTES {
            return self.reject();
        }
        self.enqueue(events)
    }

    fn enqueue(&mut self, events: Vec<Event>) -> Result<(), ()> {
        let incoming_count = events.len();
        let Some(next_count) = self.events.len().checked_add(incoming_count) else {
            return self.reject();
        };
        let incoming_bytes = events.iter().try_fold(0usize, |total, event| {
            total.checked_add(Self::event_retained_bytes(event))
        });
        let Some(incoming_bytes) = incoming_bytes else {
            return self.reject();
        };
        let Some(next_bytes) = self.retained_bytes.checked_add(incoming_bytes) else {
            return self.reject();
        };
        if next_count > PROTOCOL_BARRIER_MAX_EVENTS || next_bytes > PROTOCOL_BARRIER_MAX_BYTES {
            return self.reject();
        }

        self.events.extend(events);
        self.retained_bytes = next_bytes;
        metrics::histogram!("mux.tmux.protocol_barrier.events").record(next_count as f64);
        metrics::histogram!("mux.tmux.protocol_barrier.bytes").record(next_bytes as f64);
        Ok(())
    }

    fn reject(&mut self) -> Result<(), ()> {
        self.rejected_batches = self.rejected_batches.saturating_add(1);
        metrics::counter!("mux.tmux.protocol_barrier.rejected_batches").increment(1);
        Err(())
    }

    fn pop_front(&mut self) -> Option<Event> {
        let event = self.events.pop_front()?;
        self.retained_bytes = self
            .retained_bytes
            .saturating_sub(Self::event_retained_bytes(&event));
        Some(event)
    }

    fn response_committed(&mut self) {
        self.retained_bytes = self
            .retained_bytes
            .saturating_sub(self.response_retained_bytes);
        self.response_retained_bytes = 0;
    }

    fn clear(&mut self) -> VecDeque<Event> {
        self.active = false;
        self.retained_bytes = 0;
        self.response_retained_bytes = 0;
        std::mem::take(&mut self.events)
    }
}

#[derive(Debug)]
pub(crate) struct InFlightTmuxCommand {
    command: Box<dyn TmuxCommand>,
    generation: u64,
    remaining_responses: usize,
    first_error: Option<Guarded>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TmuxEnqueueError {
    Closed,
    Full,
    ClassMismatch,
}

impl fmt::Display for TmuxEnqueueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => f.write_str("tmux command mailbox is closed"),
            Self::Full => f.write_str("tmux command mailbox is full"),
            Self::ClassMismatch => {
                f.write_str("tmux command does not belong to the required admission class")
            }
        }
    }
}

impl std::error::Error for TmuxEnqueueError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TmuxScheduleError {
    SchedulerUnavailable,
    MuxUnavailable,
    DomainUnavailable,
    WrongDomainType,
    ReplacedDomain,
}

impl fmt::Display for TmuxScheduleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SchedulerUnavailable => {
                f.write_str("the main-thread scheduler is unavailable")
            }
            Self::MuxUnavailable => f.write_str("the active mux is unavailable"),
            Self::DomainUnavailable => f.write_str("the tmux domain is not registered"),
            Self::WrongDomainType => {
                f.write_str("the registered domain is not a tmux domain")
            }
            Self::ReplacedDomain => {
                f.write_str("the registered tmux domain is a replacement instance")
            }
        }
    }
}

impl std::error::Error for TmuxScheduleError {}

impl TmuxCmdQueue {
    pub(crate) fn new() -> Self {
        Self {
            durable_entries: VecDeque::new(),
            intent_entries: VecDeque::new(),
            in_flight: None,
            preparing: None,
            retained_by_class: [0; TmuxCommandClass::COUNT],
            durable_since_intent: 0,
            payload_bytes: 0,
            closed: false,
            terminal_barrier: false,
            terminal_command_dispatched: false,
            rejected_commands: 0,
            merged_commands: 0,
        }
    }

    /// Enqueues only while the owning tmux domain is live. Closing and pushing
    /// share the same mutex, so stale PTY/writer handles cannot refill a queue
    /// after terminal cleanup.
    pub(crate) fn push_back(&mut self, cmd: Box<dyn TmuxCommand>) -> Result<(), TmuxEnqueueError> {
        if self.closed || self.terminal_barrier {
            return Err(TmuxEnqueueError::Closed);
        }

        let class = cmd.mailbox_class();
        let incoming_payload_bytes = cmd.mailbox_payload_bytes();
        let Some(next_payload_bytes) = self.payload_bytes.checked_add(incoming_payload_bytes)
        else {
            return self.reject_full(class, "payload byte accounting overflow");
        };
        if next_payload_bytes > CMD_QUEUE_MAX_PAYLOAD_BYTES {
            return self.reject_full(class, "payload byte cap");
        }

        let merged = {
            let lane = match class {
                TmuxCommandClass::CoalescibleIntent => &mut self.intent_entries,
                TmuxCommandClass::LosslessInput
                | TmuxCommandClass::RequiredControl
                | TmuxCommandClass::TerminalControl => &mut self.durable_entries,
            };
            lane.back_mut()
                .is_some_and(|last| last.try_merge_newer(cmd.as_ref()))
        };
        if merged {
            self.payload_bytes = next_payload_bytes;
            self.merged_commands = self.merged_commands.saturating_add(1);
            metrics::counter!(
                "mux.tmux.command_mailbox.admitted",
                "class" => class.label(),
                "disposition" => "merged",
            )
            .increment(1);
            return Ok(());
        }

        if !self.can_admit_count(class, 1) {
            return self.reject_full(class, "semantic class command count cap");
        }

        match class {
            TmuxCommandClass::CoalescibleIntent => self.intent_entries.push_back(cmd),
            TmuxCommandClass::LosslessInput
            | TmuxCommandClass::RequiredControl
            | TmuxCommandClass::TerminalControl => self.durable_entries.push_back(cmd),
        }
        self.retained_by_class[class.index()] =
            self.retained_by_class[class.index()].saturating_add(1);
        self.payload_bytes = next_payload_bytes;
        metrics::counter!(
            "mux.tmux.command_mailbox.admitted",
            "class" => class.label(),
            "disposition" => "queued",
        )
        .increment(1);
        if self.len() == CMD_QUEUE_WARNING_DEPTH.saturating_add(1) {
            log::warn!(
                "tmux command queue depth exceeds {} threshold; possible protocol churn",
                CMD_QUEUE_WARNING_DEPTH
            );
        }
        Ok(())
    }

    /// Installs the one-way explicit-detach barrier at the front of the
    /// durable lane.
    ///
    /// An already preparing or in-flight command retains ownership of its
    /// response. Once that command completes, detach is the next command
    /// dispatched. No later producer may enqueue behind the terminal barrier,
    /// and work that was already behind it is abandoned by terminal cleanup
    /// after tmux emits `Exit`.
    fn push_domain_detach(
        &mut self,
        command: Box<dyn TmuxCommand>,
    ) -> Result<(), TmuxEnqueueError> {
        if self.closed || self.terminal_barrier {
            return Err(TmuxEnqueueError::Closed);
        }
        if command.mailbox_class() != TmuxCommandClass::TerminalControl
            || !command.awaits_clean_exit()
        {
            self.rejected_commands = self.rejected_commands.saturating_add(1);
            metrics::counter!(
                "mux.tmux.command_mailbox.rejected",
                "class" => TmuxCommandClass::TerminalControl.label(),
                "reason" => "class_mismatch",
            )
            .increment(1);
            return Err(TmuxEnqueueError::ClassMismatch);
        }

        let incoming_payload_bytes = command.mailbox_payload_bytes();
        let Some(next_payload_bytes) = self.payload_bytes.checked_add(incoming_payload_bytes)
        else {
            return self.reject_full(
                TmuxCommandClass::TerminalControl,
                "detach payload byte accounting overflow",
            );
        };
        if next_payload_bytes > CMD_QUEUE_MAX_PAYLOAD_BYTES {
            return self.reject_full(
                TmuxCommandClass::TerminalControl,
                "detach payload byte cap",
            );
        }
        if !self.can_admit_count(TmuxCommandClass::TerminalControl, 1) {
            return self.reject_full(
                TmuxCommandClass::TerminalControl,
                "detach command count cap",
            );
        }

        self.durable_entries.push_front(command);
        self.retained_by_class[TmuxCommandClass::TerminalControl.index()] = self.retained_by_class
            [TmuxCommandClass::TerminalControl.index()]
        .saturating_add(1);
        self.payload_bytes = next_payload_bytes;
        self.terminal_barrier = true;
        metrics::counter!(
            "mux.tmux.command_mailbox.admitted",
            "class" => TmuxCommandClass::TerminalControl.label(),
            "disposition" => "terminal_barrier",
        )
        .increment(1);
        Ok(())
    }

    /// Admit a required control-plane batch atomically. Required topology
    /// synchronization must never report success after admitting only a
    /// prefix of the batch.
    fn push_required_batch(
        &mut self,
        commands: Vec<Box<dyn TmuxCommand>>,
    ) -> Result<(), TmuxEnqueueError> {
        if self.closed || self.terminal_barrier {
            return Err(TmuxEnqueueError::Closed);
        }
        if commands.is_empty() {
            return Ok(());
        }
        if commands
            .iter()
            .any(|command| command.mailbox_class() != TmuxCommandClass::RequiredControl)
        {
            self.rejected_commands = self.rejected_commands.saturating_add(1);
            metrics::counter!(
                "mux.tmux.command_mailbox.rejected",
                "class" => TmuxCommandClass::RequiredControl.label(),
                "reason" => "class_mismatch",
            )
            .increment(1);
            return Err(TmuxEnqueueError::ClassMismatch);
        }

        let incoming_payload_bytes = commands.iter().try_fold(0usize, |total, command| {
            total.checked_add(command.mailbox_payload_bytes())
        });
        let Some(incoming_payload_bytes) = incoming_payload_bytes else {
            return self.reject_full(
                TmuxCommandClass::RequiredControl,
                "required batch payload accounting overflow",
            );
        };
        let Some(next_payload_bytes) = self.payload_bytes.checked_add(incoming_payload_bytes)
        else {
            return self.reject_full(
                TmuxCommandClass::RequiredControl,
                "required batch aggregate payload accounting overflow",
            );
        };
        if next_payload_bytes > CMD_QUEUE_MAX_PAYLOAD_BYTES {
            return self.reject_full(
                TmuxCommandClass::RequiredControl,
                "required batch payload byte cap",
            );
        }
        if !self.can_admit_count(TmuxCommandClass::RequiredControl, commands.len()) {
            return self.reject_full(
                TmuxCommandClass::RequiredControl,
                "required batch command count cap",
            );
        }

        let next_depth = self.len().saturating_add(commands.len());
        let crossed_warning_threshold =
            self.len() <= CMD_QUEUE_WARNING_DEPTH && next_depth > CMD_QUEUE_WARNING_DEPTH;
        let incoming_count = commands.len();
        self.durable_entries.extend(commands);
        self.retained_by_class[TmuxCommandClass::RequiredControl.index()] = self.retained_by_class
            [TmuxCommandClass::RequiredControl.index()]
        .saturating_add(incoming_count);
        self.payload_bytes = next_payload_bytes;
        metrics::counter!(
            "mux.tmux.command_mailbox.admitted",
            "class" => TmuxCommandClass::RequiredControl.label(),
            "disposition" => "required_batch",
        )
        .increment(u64::try_from(incoming_count).unwrap_or(u64::MAX));
        if crossed_warning_threshold {
            log::warn!(
                "tmux command queue depth exceeds {} threshold; possible protocol churn",
                CMD_QUEUE_WARNING_DEPTH
            );
        }
        Ok(())
    }

    fn can_admit_count(&self, class: TmuxCommandClass, incoming: usize) -> bool {
        let Some(next_total) = self.len().checked_add(incoming) else {
            return false;
        };
        if next_total > CMD_QUEUE_MAX_DEPTH {
            return false;
        }

        let terminal = self.retained_by_class[TmuxCommandClass::TerminalControl.index()];
        let Some(non_terminal) = self.len().checked_sub(terminal) else {
            return false;
        };
        let lossless = self.retained_by_class[TmuxCommandClass::LosslessInput.index()];
        let intent = self.retained_by_class[TmuxCommandClass::CoalescibleIntent.index()];
        let Some(non_control) = lossless.checked_add(intent) else {
            return false;
        };
        let non_terminal_capacity = non_terminal
            .checked_add(incoming)
            .is_some_and(|next| next <= CMD_QUEUE_MAX_DEPTH - CMD_QUEUE_TERMINAL_RESERVED_SLOTS);

        match class {
            TmuxCommandClass::TerminalControl => true,
            TmuxCommandClass::RequiredControl => non_terminal_capacity,
            TmuxCommandClass::LosslessInput => {
                non_terminal_capacity
                    && non_control.checked_add(incoming).is_some_and(|next| {
                        next <= CMD_QUEUE_MAX_DEPTH - CMD_QUEUE_CONTROL_RESERVED_SLOTS
                    })
            }
            TmuxCommandClass::CoalescibleIntent => {
                let class_count =
                    self.retained_by_class[TmuxCommandClass::CoalescibleIntent.index()];
                non_terminal_capacity
                    && class_count
                        .checked_add(incoming)
                        .is_some_and(|next| next <= CMD_QUEUE_INTENT_MAX_DEPTH)
                    && non_control.checked_add(incoming).is_some_and(|next| {
                        next <= CMD_QUEUE_MAX_DEPTH - CMD_QUEUE_CONTROL_RESERVED_SLOTS
                    })
            }
        }
    }

    fn reject_full(
        &mut self,
        class: TmuxCommandClass,
        reason: &str,
    ) -> Result<(), TmuxEnqueueError> {
        self.rejected_commands = self.rejected_commands.saturating_add(1);
        metrics::counter!(
            "mux.tmux.command_mailbox.rejected",
            "class" => class.label(),
            "reason" => "capacity",
        )
        .increment(1);
        if self.rejected_commands.is_power_of_two() {
            log::error!(
                "tmux command queue rejected {} work at {reason}; depth={}, payload_bytes={}, \
                 rejected={} in this queue lifetime",
                class.label(),
                self.len(),
                self.payload_bytes,
                self.rejected_commands
            );
        }
        Err(TmuxEnqueueError::Full)
    }

    pub(crate) fn close(
        &mut self,
    ) -> (VecDeque<Box<dyn TmuxCommand>>, Option<InFlightTmuxCommand>) {
        self.closed = true;
        self.preparing = None;
        self.retained_by_class = [0; TmuxCommandClass::COUNT];
        self.durable_since_intent = 0;
        self.payload_bytes = 0;
        let mut abandoned = std::mem::take(&mut self.durable_entries);
        abandoned.extend(std::mem::take(&mut self.intent_entries));
        (abandoned, self.in_flight.take())
    }

    #[cfg(test)]
    pub(crate) fn is_closed(&self) -> bool {
        self.closed
    }

    pub(crate) fn len(&self) -> usize {
        self.retained_by_class
            .iter()
            .copied()
            .fold(0usize, usize::saturating_add)
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.durable_entries.is_empty()
            && self.intent_entries.is_empty()
            && self.in_flight.is_none()
            && self.preparing.is_none()
    }

    #[cfg(test)]
    pub(crate) fn front(&self) -> Option<&dyn TmuxCommand> {
        self.in_flight
            .as_ref()
            .map(|in_flight| in_flight.command.as_ref())
            .or_else(|| {
                if self.should_service_intent() {
                    self.intent_entries.front().map(Box::as_ref)
                } else {
                    self.durable_entries.front().map(Box::as_ref)
                }
            })
    }

    fn take_next_for_preparation(&mut self) -> Option<Box<dyn TmuxCommand>> {
        debug_assert!(self.preparing.is_none());
        let service_intent = self.should_service_intent();
        let command = if service_intent {
            self.durable_since_intent = 0;
            self.intent_entries.pop_front()?
        } else {
            self.durable_since_intent = self.durable_since_intent.saturating_add(1);
            self.durable_entries.pop_front()?
        };
        if command.awaits_clean_exit() {
            debug_assert!(self.terminal_barrier);
            debug_assert!(!self.terminal_command_dispatched);
            self.terminal_command_dispatched = true;
        }
        self.preparing = Some(PreparingTmuxCommand {
            class: command.mailbox_class(),
            payload_bytes: command.mailbox_payload_bytes(),
        });
        metrics::counter!(
            "mux.tmux.command_mailbox.serviced",
            "class" => command.mailbox_class().label(),
        )
        .increment(1);
        Some(command)
    }

    fn release_prepared(&mut self) {
        if let Some(preparing) = self.preparing.take() {
            self.payload_bytes = self
                .payload_bytes
                .saturating_sub(preparing.payload_bytes);
            self.retained_by_class[preparing.class.index()] =
                self.retained_by_class[preparing.class.index()].saturating_sub(1);
        }
    }

    fn install_in_flight(&mut self, cmd: Box<dyn TmuxCommand>, generation: u64) -> bool {
        if self.closed || self.in_flight.is_some() {
            self.release_prepared();
            false
        } else {
            debug_assert_eq!(
                self.preparing.as_ref().map(|preparing| (
                    preparing.class,
                    preparing.payload_bytes
                )),
                Some((cmd.mailbox_class(), cmd.mailbox_payload_bytes()))
            );
            self.preparing = None;
            let remaining_responses = cmd.expected_responses();
            debug_assert!(remaining_responses > 0);
            self.in_flight = Some(InFlightTmuxCommand {
                command: cmd,
                generation,
                remaining_responses,
                first_error: None,
            });
            true
        }
    }

    fn record_in_flight_response(
        &mut self,
        response: &Guarded,
    ) -> Option<(Box<dyn TmuxCommand>, Guarded, u64)> {
        let in_flight = self.in_flight.as_mut()?;
        if response.error && in_flight.first_error.is_none() {
            in_flight.first_error = Some(response.clone());
        }
        in_flight.remaining_responses = in_flight.remaining_responses.saturating_sub(1);
        if in_flight.remaining_responses > 0 {
            return None;
        }

        let mut in_flight = self.in_flight.take()?;
        let class = in_flight.command.mailbox_class();
        self.retained_by_class[class.index()] =
            self.retained_by_class[class.index()].saturating_sub(1);
        self.payload_bytes = self
            .payload_bytes
            .saturating_sub(in_flight.command.mailbox_payload_bytes());
        let response = in_flight
            .first_error
            .take()
            .unwrap_or_else(|| response.clone());
        Some((in_flight.command, response, in_flight.generation))
    }

    fn has_pending(&self) -> bool {
        if self.terminal_barrier && self.terminal_command_dispatched {
            return false;
        }
        !self.durable_entries.is_empty() || !self.intent_entries.is_empty()
    }

    fn has_domain_detach_pending(&self) -> bool {
        self.terminal_barrier
    }

    fn should_service_intent(&self) -> bool {
        if self.terminal_barrier && !self.terminal_command_dispatched {
            return false;
        }
        !self.intent_entries.is_empty()
            && (self.durable_entries.is_empty()
                || self.durable_since_intent >= CMD_QUEUE_DURABLE_SERVICE_QUANTUM)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TmuxIoOperationKind {
    Command,
    Detach,
}

impl TmuxIoOperationKind {
    const fn label(self) -> &'static str {
        match self {
            Self::Command => "command",
            Self::Detach => "detach",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TmuxIoOperationPhase {
    WaitingForResponse,
    WaitingForCleanExit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TmuxIoOperationLease {
    generation: u64,
    kind: TmuxIoOperationKind,
    phase: TmuxIoOperationPhase,
}

#[derive(Clone, Copy, Debug)]
struct TmuxIoDeadlines {
    start: Duration,
    write: Duration,
    response: Duration,
}

impl TmuxIoDeadlines {
    fn current() -> Self {
        let config = configuration();
        Self {
            start: Duration::from_millis(config.mux_tmux_io_start_timeout_ms),
            write: Duration::from_millis(config.mux_tmux_io_write_timeout_ms),
            response: Duration::from_millis(config.mux_tmux_response_timeout_ms),
        }
    }
}

struct TmuxIoStart {
    generation: u64,
    kind: TmuxIoOperationKind,
    command: Option<String>,
    admitted_at: Instant,
    deadlines: TmuxIoDeadlines,
    _operation: OwnedActiveTmuxOperation,
}

impl fmt::Debug for TmuxIoStart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TmuxIoStart")
            .field("generation", &self.generation)
            .field("kind", &self.kind)
            .field(
                "command_bytes",
                &self.command.as_ref().map_or(0, String::len),
            )
            .field("admitted_at", &self.admitted_at)
            .field("deadlines", &self.deadlines)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
enum TmuxIoControl {
    Start(TmuxIoStart),
    InitialGuardReady,
    #[cfg(test)]
    TestInitialGuardDeadline(Duration),
    Response { generation: u64 },
    Terminal { clean_exit: bool },
}

#[derive(Debug)]
struct TmuxIoWriteJob {
    generation: u64,
    kind: TmuxIoOperationKind,
    command: Option<String>,
}

#[derive(Debug)]
enum TmuxIoWriteOutcome {
    Succeeded,
    OwnerGone,
    RegistrationLost,
    LauncherPaneGone,
    LauncherBindingReplaced,
    Io {
        error_kind: std::io::ErrorKind,
        message: String,
    },
    Panicked,
}

impl TmuxIoWriteOutcome {
    const fn reason_label(&self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::OwnerGone => "owner_gone",
            Self::RegistrationLost => "registration_lost",
            Self::LauncherPaneGone => "launcher_pane_gone",
            Self::LauncherBindingReplaced => "launcher_binding_replaced",
            Self::Io { .. } => "io_error",
            Self::Panicked => "panicked",
        }
    }

    fn detail(&self) -> String {
        match self {
            Self::Io {
                error_kind,
                message,
            } => format!("{error_kind:?}: {message}"),
            other => other.reason_label().to_string(),
        }
    }
}

#[derive(Debug)]
enum TmuxIoWriteProgress {
    Started {
        generation: u64,
        kind: TmuxIoOperationKind,
    },
    Finished {
        generation: u64,
        kind: TmuxIoOperationKind,
        outcome: TmuxIoWriteOutcome,
    },
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum TmuxIoAdmissionError {
    Unavailable,
    Full,
    Disconnected,
}

impl fmt::Display for TmuxIoAdmissionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => f.write_str("tmux I/O supervisor is unavailable"),
            Self::Full => f.write_str("tmux I/O supervisor control lane is full"),
            Self::Disconnected => f.write_str("tmux I/O supervisor has disconnected"),
        }
    }
}

impl std::error::Error for TmuxIoAdmissionError {}

struct TmuxIoLane {
    control: Sender<TmuxIoControl>,
}

impl TmuxIoLane {
    fn new(
        domain_id: DomainId,
        owner: Weak<TmuxDomainState>,
    ) -> std::io::Result<Self> {
        let (control_tx, control_rx) = bounded(TMUX_IO_CONTROL_CAPACITY);
        let thread_name = format!("tmux-io-supervisor-{domain_id}");
        let failure_owner = owner.clone();
        let spawn_result = std::thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_tmux_io_supervisor(owner, control_rx);
                }));
                if outcome.is_err() {
                    log::error!("tmux I/O supervisor for domain {domain_id} panicked");
                    if let Some(domain) = failure_owner.upgrade() {
                        domain.fail_io_supervisor("supervisor_panicked");
                    }
                }
            });
        Self::from_spawn_result(domain_id, control_tx, spawn_result)
    }

    fn from_spawn_result(
        domain_id: DomainId,
        control: Sender<TmuxIoControl>,
        spawn_result: std::io::Result<std::thread::JoinHandle<()>>,
    ) -> std::io::Result<Self> {
        match spawn_result {
            Ok(_) => Ok(Self { control }),
            Err(err) => {
                log::error!("failed to start tmux I/O supervisor for domain {domain_id}: {err:#}");
                Err(err)
            }
        }
    }

    fn start(&self, start: TmuxIoStart) -> Result<(), TmuxIoAdmissionError> {
        match self.control.try_send(TmuxIoControl::Start(start)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(TmuxIoAdmissionError::Full),
            Err(TrySendError::Disconnected(_)) => Err(TmuxIoAdmissionError::Disconnected),
        }
    }

    fn signal_response(&self, generation: u64) -> Result<(), TmuxIoAdmissionError> {
        match self
            .control
            .try_send(TmuxIoControl::Response { generation })
        {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(TmuxIoAdmissionError::Full),
            Err(TrySendError::Disconnected(_)) => Err(TmuxIoAdmissionError::Disconnected),
        }
    }

    fn signal_initial_guard_ready(&self) -> Result<(), TmuxIoAdmissionError> {
        match self.control.try_send(TmuxIoControl::InitialGuardReady) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(TmuxIoAdmissionError::Full),
            Err(TrySendError::Disconnected(_)) => Err(TmuxIoAdmissionError::Disconnected),
        }
    }

    #[cfg(test)]
    fn set_test_initial_guard_deadline(
        &self,
        deadline: Duration,
    ) -> Result<(), TmuxIoAdmissionError> {
        match self
            .control
            .try_send(TmuxIoControl::TestInitialGuardDeadline(deadline))
        {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(TmuxIoAdmissionError::Full),
            Err(TrySendError::Disconnected(_)) => Err(TmuxIoAdmissionError::Disconnected),
        }
    }

    fn signal_terminal(&self, clean_exit: bool) {
        let _ = self
            .control
            .try_send(TmuxIoControl::Terminal { clean_exit });
    }
}

struct TmuxIoWriter {
    jobs: Sender<TmuxIoWriteJob>,
    progress: Receiver<TmuxIoWriteProgress>,
}

fn start_tmux_io_writer(
    domain_id: DomainId,
    owner: Weak<TmuxDomainState>,
) -> Result<TmuxIoWriter, String> {
    let (job_tx, job_rx) = bounded(TMUX_IO_WRITE_CAPACITY);
    let (progress_tx, progress_rx) = bounded(TMUX_IO_RESULT_CAPACITY);
    let thread_name = format!("tmux-io-writer-{domain_id}");
    std::thread::Builder::new()
        .name(thread_name)
        .spawn(move || run_tmux_io_writer(owner, job_rx, progress_tx))
        .map_err(|err| format!("failed to start tmux I/O writer: {err}"))?;
    Ok(TmuxIoWriter {
        jobs: job_tx,
        progress: progress_rx,
    })
}

fn run_tmux_io_writer(
    owner: Weak<TmuxDomainState>,
    jobs: Receiver<TmuxIoWriteJob>,
    progress: Sender<TmuxIoWriteProgress>,
) {
    while let Ok(job) = jobs.recv() {
        if progress
            .send(TmuxIoWriteProgress::Started {
                generation: job.generation,
                kind: job.kind,
            })
            .is_err()
        {
            return;
        }
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            execute_tmux_io_write(&owner, &job)
        }))
        .unwrap_or(TmuxIoWriteOutcome::Panicked);
        if progress
            .send(TmuxIoWriteProgress::Finished {
                generation: job.generation,
                kind: job.kind,
                outcome,
            })
            .is_err()
        {
            return;
        }
    }
}

fn execute_tmux_io_write(
    owner: &Weak<TmuxDomainState>,
    job: &TmuxIoWriteJob,
) -> TmuxIoWriteOutcome {
    let Some(owner) = owner.upgrade() else {
        return TmuxIoWriteOutcome::OwnerGone;
    };
    let Some(mux) = Mux::try_get() else {
        return TmuxIoWriteOutcome::RegistrationLost;
    };
    let Some(domain) = mux.get_domain(owner.domain_id) else {
        return TmuxIoWriteOutcome::RegistrationLost;
    };
    let Some(tmux_domain) = domain.downcast_ref::<TmuxDomain>() else {
        return TmuxIoWriteOutcome::RegistrationLost;
    };
    if !Arc::ptr_eq(&owner, &tmux_domain.inner) {
        return TmuxIoWriteOutcome::RegistrationLost;
    }
    let Some(pane) = mux.get_pane(owner.pane_id) else {
        return TmuxIoWriteOutcome::LauncherPaneGone;
    };
    let Some(command) = job.command.as_ref() else {
        return TmuxIoWriteOutcome::Io {
            error_kind: std::io::ErrorKind::InvalidInput,
            message: format!(
                "tmux {} I/O job had no command payload",
                job.kind.label()
            ),
        };
    };

    let result: anyhow::Result<()> = match job.kind {
        TmuxIoOperationKind::Command => pane
            .writer()
            .write_all(command.as_bytes())
            .map_err(anyhow::Error::from),
        TmuxIoOperationKind::Detach => {
            if let Some(local_pane) = pane.downcast_ref::<LocalPane>() {
                match local_pane.write_tmux_command_if_same(&owner, command) {
                    Ok(true) => Ok(()),
                    Ok(false) => return TmuxIoWriteOutcome::LauncherBindingReplaced,
                    Err(err) => Err(err),
                }
            } else {
                if owner.is_terminal() {
                    return TmuxIoWriteOutcome::RegistrationLost;
                }
                pane.writer()
                    .write_all(command.as_bytes())
                    .map_err(anyhow::Error::from)
            }
        }
    };
    match result {
        Ok(()) => TmuxIoWriteOutcome::Succeeded,
        Err(err) => TmuxIoWriteOutcome::Io {
            error_kind: err
                .downcast_ref::<std::io::Error>()
                .map_or(std::io::ErrorKind::Other, std::io::Error::kind),
            message: err.to_string(),
        },
    }
}

fn remaining_deadline(started: Instant, budget: Duration) -> Option<Duration> {
    budget.checked_sub(started.elapsed())
}

fn recv_tmux_io_progress(
    receiver: &Receiver<TmuxIoWriteProgress>,
    timeout: Duration,
) -> Option<TmuxIoWriteProgress> {
    match receiver.recv_timeout(timeout) {
        Ok(progress) => Some(progress),
        Err(crossbeam::channel::RecvTimeoutError::Timeout) => receiver.try_recv().ok(),
        Err(crossbeam::channel::RecvTimeoutError::Disconnected) => None,
    }
}

fn run_tmux_io_supervisor(
    owner: Weak<TmuxDomainState>,
    control: Receiver<TmuxIoControl>,
) {
    let Some(initial_owner) = owner.upgrade() else {
        return;
    };
    let initial_guard_budget = Cell::new(initial_owner.io_deadlines().response);
    drop(initial_owner);

    let initial_guard_started = Cell::new(Instant::now());
    let mut awaiting_initial_guard = true;
    let mut writer: Option<TmuxIoWriter> = None;
    loop {
        let message = if awaiting_initial_guard {
            let Some(remaining) = remaining_deadline(
                initial_guard_started.get(),
                initial_guard_budget.get(),
            )
            else {
                let Some(domain) = owner.upgrade() else {
                    return;
                };
                let guard_is_pending = *domain.state.lock() == State::WaitForInitialGuard;
                if guard_is_pending {
                    domain.fail_initial_guard("response_timeout");
                    return;
                }
                // The parser commits the state transition before publishing
                // the readiness signal. If the deadline lands in that narrow
                // interval, accept the committed state and let the late signal
                // be treated as stale instead of tearing down a healthy
                // domain.
                awaiting_initial_guard = false;
                continue;
            };
            match control.recv_timeout(remaining) {
                Ok(message) => message,
                Err(crossbeam::channel::RecvTimeoutError::Timeout) => {
                    let Some(domain) = owner.upgrade() else {
                        return;
                    };
                    let guard_is_pending = *domain.state.lock() == State::WaitForInitialGuard;
                    if guard_is_pending {
                        domain.fail_initial_guard("response_timeout");
                        return;
                    }
                    awaiting_initial_guard = false;
                    continue;
                }
                Err(crossbeam::channel::RecvTimeoutError::Disconnected) => return,
            }
        } else {
            let Ok(message) = control.recv() else {
                return;
            };
            message
        };

        match message {
            TmuxIoControl::Start(mut start) => {
                let Some(domain) = owner.upgrade() else {
                    return;
                };
                if awaiting_initial_guard {
                    let guard_is_pending = *domain.state.lock() == State::WaitForInitialGuard;
                    if guard_is_pending {
                        domain.fail_initial_guard("operation_before_initial_guard");
                        return;
                    }
                    // Unit tests and recovery paths may establish an already
                    // committed non-initial state directly. The operation
                    // generation and state checks below remain authoritative.
                    awaiting_initial_guard = false;
                }
                if !domain.io_operation_is_current(start.kind, start.generation) {
                    continue;
                }
                if remaining_deadline(start.admitted_at, start.deadlines.start).is_none() {
                    domain.fail_tmux_io_operation(
                        start.kind,
                        start.generation,
                        "start_timeout",
                    );
                    return;
                };
                if writer.is_none() {
                    match start_tmux_io_writer(domain.domain_id, Arc::downgrade(&domain)) {
                        Ok(started) => writer = Some(started),
                        Err(err) => {
                            log::error!(
                                "tmux domain {} could not start its bounded I/O writer: {err}",
                                domain.domain_id
                            );
                            domain.fail_tmux_io_operation(
                                start.kind,
                                start.generation,
                                "writer_unavailable",
                            );
                            return;
                        }
                    }
                }
                let Some(remaining_start) =
                    remaining_deadline(start.admitted_at, start.deadlines.start)
                else {
                    domain.fail_tmux_io_operation(
                        start.kind,
                        start.generation,
                        "start_timeout",
                    );
                    return;
                };
                let writer_ref = writer.as_ref().expect("tmux writer initialized");
                let job = TmuxIoWriteJob {
                    generation: start.generation,
                    kind: start.kind,
                    command: start.command.take(),
                };
                if writer_ref.jobs.try_send(job).is_err() {
                    domain.fail_tmux_io_operation(
                        start.kind,
                        start.generation,
                        "writer_backpressured",
                    );
                    return;
                }
                let Some(started) = recv_tmux_io_progress(&writer_ref.progress, remaining_start)
                else {
                    domain.fail_tmux_io_operation(
                        start.kind,
                        start.generation,
                        "start_timeout",
                    );
                    return;
                };
                if !matches!(
                    started,
                    TmuxIoWriteProgress::Started { generation, kind }
                        if generation == start.generation && kind == start.kind
                ) {
                    domain.fail_tmux_io_operation(
                        start.kind,
                        start.generation,
                        "start_protocol_mismatch",
                    );
                    return;
                }

                let write_started = Instant::now();
                let Some(finished) =
                    recv_tmux_io_progress(&writer_ref.progress, start.deadlines.write)
                else {
                    domain.fail_tmux_io_operation(
                        start.kind,
                        start.generation,
                        "write_timeout",
                    );
                    return;
                };
                let TmuxIoWriteProgress::Finished {
                    generation,
                    kind,
                    outcome,
                } = finished
                else {
                    domain.fail_tmux_io_operation(
                        start.kind,
                        start.generation,
                        "write_protocol_mismatch",
                    );
                    return;
                };
                if generation != start.generation || kind != start.kind {
                    domain.fail_tmux_io_operation(
                        start.kind,
                        start.generation,
                        "write_protocol_mismatch",
                    );
                    return;
                }
                metrics::histogram!(
                    "mux.tmux.io.write_seconds",
                    "operation" => start.kind.label(),
                )
                .record(write_started.elapsed().as_secs_f64());
                if !matches!(outcome, TmuxIoWriteOutcome::Succeeded) {
                    log::error!(
                        "tmux domain {} {} generation {} failed on its bounded I/O lane: {}",
                        domain.domain_id,
                        start.kind.label(),
                        start.generation,
                        outcome.detail()
                    );
                    domain.fail_tmux_io_operation(
                        start.kind,
                        start.generation,
                        outcome.reason_label(),
                    );
                    return;
                }
                metrics::counter!(
                    "mux.tmux.io.completed",
                    "operation" => start.kind.label(),
                    "outcome" => "write_succeeded",
                )
                .increment(1);

                let kind = start.kind;
                let generation = start.generation;
                let response_budget = start.deadlines.response;
                drop(start);
                if domain.is_terminal() {
                    return;
                }

                let response_started = Instant::now();
                let mut guarded_response_received = false;
                loop {
                    let Some(remaining) = remaining_deadline(response_started, response_budget)
                    else {
                        if domain.io_operation_is_current(kind, generation) {
                            domain.fail_tmux_io_operation(
                                kind,
                                generation,
                                if guarded_response_received {
                                    "clean_exit_timeout"
                                } else {
                                    "response_timeout"
                                },
                            );
                        }
                        return;
                    };
                    match control.recv_timeout(remaining) {
                        Ok(TmuxIoControl::Response {
                            generation: response_generation,
                        }) if response_generation == generation && !guarded_response_received =>
                        {
                            metrics::histogram!(
                                "mux.tmux.io.response_seconds",
                                "operation" => kind.label(),
                            )
                            .record(response_started.elapsed().as_secs_f64());
                            if kind == TmuxIoOperationKind::Command {
                                break;
                            }
                            guarded_response_received = true;
                        }
                        Ok(TmuxIoControl::Terminal { clean_exit: true })
                            if kind == TmuxIoOperationKind::Detach =>
                        {
                            metrics::histogram!(
                                "mux.tmux.io.clean_exit_seconds",
                            )
                            .record(response_started.elapsed().as_secs_f64());
                            return;
                        }
                        Ok(TmuxIoControl::Terminal { .. }) => return,
                        Ok(TmuxIoControl::Response { .. }) => {
                            metrics::counter!(
                                "mux.tmux.io.stale_signal",
                                "operation" => kind.label(),
                            )
                            .increment(1);
                        }
                        Ok(TmuxIoControl::Start(_)) => {
                            domain.fail_tmux_io_operation(
                                kind,
                                generation,
                                "overlapping_start",
                            );
                            return;
                        }
                        Ok(TmuxIoControl::InitialGuardReady) => {
                            metrics::counter!(
                                "mux.tmux.io.stale_signal",
                                "operation" => kind.label(),
                            )
                            .increment(1);
                        }
                        #[cfg(test)]
                        Ok(TmuxIoControl::TestInitialGuardDeadline(_)) => {
                            metrics::counter!(
                                "mux.tmux.io.stale_signal",
                                "operation" => kind.label(),
                            )
                            .increment(1);
                        }
                        Err(crossbeam::channel::RecvTimeoutError::Timeout) => {
                            if domain.io_operation_is_current(kind, generation) {
                                domain.fail_tmux_io_operation(
                                    kind,
                                    generation,
                                    if guarded_response_received {
                                        "clean_exit_timeout"
                                    } else {
                                        "response_timeout"
                                    },
                                );
                            }
                            return;
                        }
                        Err(crossbeam::channel::RecvTimeoutError::Disconnected) => return,
                    }
                }
            }
            TmuxIoControl::InitialGuardReady => {
                if awaiting_initial_guard {
                    awaiting_initial_guard = false;
                    metrics::histogram!(
                        "mux.tmux.io.response_seconds",
                        "operation" => "initial_guard",
                    )
                    .record(initial_guard_started.get().elapsed().as_secs_f64());
                } else {
                    metrics::counter!(
                        "mux.tmux.io.stale_signal",
                        "operation" => "initial_guard",
                    )
                    .increment(1);
                }
            }
            #[cfg(test)]
            TmuxIoControl::TestInitialGuardDeadline(deadline) => {
                if awaiting_initial_guard {
                    initial_guard_budget.set(deadline);
                    initial_guard_started.set(Instant::now());
                }
            }
            TmuxIoControl::Response { .. } => {
                metrics::counter!("mux.tmux.io.stale_signal", "operation" => "idle").increment(1);
            }
            TmuxIoControl::Terminal { .. } => return,
        }
    }
}

pub(crate) struct TmuxDomainState {
    pub pane_id: PaneId,     // ID of the original pane
    pub domain_id: DomainId, // ID of TmuxDomain
    state: Mutex<State>,
    lifecycle: Mutex<TmuxLifecycle>,
    clean_exit_requested: Arc<AtomicBool>,
    clean_detach_completed: AtomicBool,
    detach_cleanup_scheduled: Arc<AtomicBool>,
    send_task_scheduled: Arc<AtomicBool>,
    io_lane: OnceLock<TmuxIoLane>,
    next_io_generation: AtomicU64,
    pub cmd_queue: Arc<Mutex<TmuxCmdQueue>>,
    protocol_ingress: Mutex<()>,
    protocol_barrier: Mutex<TmuxProtocolBarrier>,
    pub gui_window: Mutex<Option<MuxWindowBuilder>>,
    pub gui_tabs: Mutex<HashMap<TmuxWindowId, TmuxTab>>,
    pub remote_panes: Mutex<HashMap<TmuxPaneId, RefTmuxRemotePane>>,
    pub(crate) mirror_index: Mutex<TmuxMirrorIndex>,
    pub(crate) notification_intents: Mutex<TmuxNotificationIntentState>,
    pub(crate) notification_intent_telemetry: TmuxNotificationIntentTelemetry,
    pub(crate) pane_retirement: Mutex<()>,
    pub(crate) retired_panes: Mutex<HashSet<TmuxPaneId>>,
    pub tmux_session: Mutex<Option<TmuxSessionId>>,
    pub support_commands: Mutex<HashMap<String, String>>,
    pub attach_state: Mutex<AttachState>,
    pub(crate) notification_subscription_gate: Mutex<()>,
    pub notification_sub_id: Mutex<Option<usize>>,
    config_reload_sub: Mutex<Option<config::ConfigSubscription>>,
    backlog_limits_dirty: AtomicBool,
    pending_splits: Mutex<HashMap<u64, promise::Promise<TmuxPaneId>>>,
    next_split_request_id: AtomicU64,
    pub backlog: Mutex<TmuxBacklog>,
    #[cfg(test)]
    test_io_deadlines: Mutex<Option<TmuxIoDeadlines>>,
}

pub struct TmuxDomain {
    pub(crate) inner: Arc<TmuxDomainState>,
}

#[derive(Debug, Default)]
struct TmuxLifecycle {
    terminal: bool,
    clean_exit: bool,
    io_operation: Option<TmuxIoOperationLease>,
    active_operations: usize,
    cleanup_in_progress: bool,
    resources_cleaned: bool,
    finalization_in_progress: bool,
    finalized: bool,
    detach_disposition: TerminalDetachDisposition,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum TerminalDetachDisposition {
    #[default]
    NotNeeded,
    Pending,
    Claimed,
    Attempted,
}

struct ActiveTmuxOperation<'a> {
    owner: &'a TmuxDomainState,
}

impl Drop for ActiveTmuxOperation<'_> {
    fn drop(&mut self) {
        self.owner.finish_active_operation();
    }
}

struct OwnedActiveTmuxOperation {
    owner: Arc<TmuxDomainState>,
}

impl Drop for OwnedActiveTmuxOperation {
    fn drop(&mut self) {
        self.owner.finish_active_operation();
    }
}

struct DetachScheduleLease {
    scheduled: Arc<AtomicBool>,
    completed: bool,
}

impl Drop for DetachScheduleLease {
    fn drop(&mut self) {
        if !self.completed {
            self.scheduled.store(false, Ordering::Release);
        }
    }
}

struct SendScheduleLease {
    owner: Arc<TmuxDomainState>,
    completed: bool,
}

impl Drop for SendScheduleLease {
    fn drop(&mut self) {
        self.owner
            .send_task_scheduled
            .store(false, Ordering::Release);
        if !self.completed && !self.owner.is_terminal() {
            log::error!(
                "tmux domain {} lost its scheduled sender runnable; detaching instead of \
                 stranding durably admitted commands",
                self.owner.domain_id
            );
            self.owner.transition_to_exit_and_schedule_detach();
        }
    }
}

struct ResponseBarrierLease {
    owner: Arc<TmuxDomainState>,
    completed: bool,
}

impl Drop for ResponseBarrierLease {
    fn drop(&mut self) {
        if !self.completed {
            log::error!(
                "tmux domain {} lost its scheduled command-result task; detaching instead of \
                 stranding the protocol barrier",
                self.owner.domain_id
            );
            self.owner.transition_to_exit_and_schedule_detach();
        }
    }
}

impl TmuxDomainState {
    pub(crate) fn registered_owner_weak(&self) -> anyhow::Result<Weak<Self>> {
        let mux = Mux::try_get().context("tmux domain requires active mux")?;
        let domain = mux
            .get_domain(self.domain_id)
            .context("tmux domain is not registered")?;
        let tmux_domain = domain
            .downcast_ref::<TmuxDomain>()
            .context("registered domain is not a tmux domain")?;
        anyhow::ensure!(
            std::ptr::eq(tmux_domain.inner.as_ref(), self),
            "registered tmux domain instance does not match pane owner"
        );
        Ok(Arc::downgrade(&tmux_domain.inner))
    }

    pub(crate) fn transition_to_exit_and_schedule_detach(&self) {
        self.request_terminal(false);
    }

    /// Marks a control-mode domain terminal when the launcher has already
    /// observed a clean tmux exit. Lifecycle cleanup owns mux removal so it can
    /// wait for operations admitted before the exit without blocking the parser
    /// thread that observed the exit.
    pub(crate) fn transition_to_clean_exit(&self) {
        self.request_terminal(true);
    }

    fn request_terminal(&self, requested_clean_exit: bool) {
        let (first_transition, authoritative_clean_exit) = {
            let mut lifecycle = self.lifecycle.lock();
            if lifecycle.terminal {
                (false, lifecycle.clean_exit)
            } else {
                let mut state = self.state.lock();
                lifecycle.terminal = true;
                lifecycle.clean_exit = requested_clean_exit;
                lifecycle.io_operation = None;
                lifecycle.detach_disposition = if requested_clean_exit {
                    TerminalDetachDisposition::NotNeeded
                } else {
                    TerminalDetachDisposition::Pending
                };
                *state = State::Exit;
                if requested_clean_exit {
                    self.clean_exit_requested.store(true, Ordering::Release);
                }
                (true, requested_clean_exit)
            }
        };

        self.publish_terminal_transition(first_transition, authoritative_clean_exit);
    }

    fn publish_terminal_transition(&self, first_transition: bool, clean_exit: bool) {
        if let Some(io_lane) = self.io_lane.get() {
            io_lane.signal_terminal(clean_exit);
        }

        // Queue closure and unsubscription are immediate. More expensive
        // resource cleanup is deferred until operations admitted before the
        // terminal transition have drained.
        self.notification_intents.lock().close();
        let abandoned_commands = { self.cmd_queue.lock().close() };
        // Command destructors can release large paste buffers (and may grow
        // richer in the future); never run them while producers need the
        // mailbox mutex to observe closure.
        drop(abandoned_commands);
        let abandoned_protocol_events = { self.protocol_barrier.lock().clear() };
        // Event payloads and the deque's high-water allocation can both be
        // large. Drop them after releasing the protocol-barrier mutex.
        drop(abandoned_protocol_events);
        self.unsubscribe_notification();

        if first_transition {
            log::debug!(
                "tmux domain {} entered {}terminal state",
                self.domain_id,
                if clean_exit { "clean " } else { "" }
            );
        }
        self.finish_terminal_cleanup_if_ready();
    }

    fn begin_active_operation(&self) -> Option<ActiveTmuxOperation<'_>> {
        let mut lifecycle = self.lifecycle.lock();
        if lifecycle.terminal {
            return None;
        }
        let Some(next) = lifecycle.active_operations.checked_add(1) else {
            log::error!(
                "tmux domain {} active-operation counter overflow; rejecting work",
                self.domain_id
            );
            return None;
        };
        lifecycle.active_operations = next;
        Some(ActiveTmuxOperation { owner: self })
    }

    fn begin_owned_operation(self: &Arc<Self>) -> Option<OwnedActiveTmuxOperation> {
        let mut lifecycle = self.lifecycle.lock();
        if lifecycle.terminal {
            return None;
        }
        let Some(next) = lifecycle.active_operations.checked_add(1) else {
            log::error!(
                "tmux domain {} active-operation counter overflow; rejecting owned work",
                self.domain_id
            );
            return None;
        };
        lifecycle.active_operations = next;
        Some(OwnedActiveTmuxOperation {
            owner: Arc::clone(self),
        })
    }

    fn alloc_io_generation(&self) -> anyhow::Result<u64> {
        self.next_io_generation
            .try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(1)
            })
            .map_err(|_| {
                anyhow::anyhow!(
                    "tmux domain {} exhausted its nonwrapping I/O generation space",
                    self.domain_id
                )
            })
    }

    fn io_deadlines(&self) -> TmuxIoDeadlines {
        #[cfg(test)]
        if let Some(deadlines) = *self.test_io_deadlines.lock() {
            return deadlines;
        }
        TmuxIoDeadlines::current()
    }

    fn io_operation_is_current(&self, kind: TmuxIoOperationKind, generation: u64) -> bool {
        let lifecycle = self.lifecycle.lock();
        !lifecycle.terminal
            && lifecycle.io_operation.is_some_and(|operation| {
                operation.kind == kind && operation.generation == generation
            })
    }

    fn install_io_operation(
        &self,
        kind: TmuxIoOperationKind,
        generation: u64,
    ) -> bool {
        let mut lifecycle = self.lifecycle.lock();
        if lifecycle.terminal || lifecycle.io_operation.is_some() {
            return false;
        }
        lifecycle.io_operation = Some(TmuxIoOperationLease {
            generation,
            kind,
            phase: TmuxIoOperationPhase::WaitingForResponse,
        });
        true
    }

    fn claim_io_response(
        &self,
        kind: TmuxIoOperationKind,
        generation: u64,
    ) -> bool {
        let mut lifecycle = self.lifecycle.lock();
        let Some(operation) = lifecycle.io_operation else {
            return false;
        };
        if lifecycle.terminal
            || operation.kind != kind
            || operation.generation != generation
            || operation.phase != TmuxIoOperationPhase::WaitingForResponse
        {
            return false;
        }

        let mut state = self.state.lock();
        if *state != State::WaitingForResponse {
            return false;
        }
        *state = State::ProcessingResponse;
        if kind == TmuxIoOperationKind::Detach {
            lifecycle.io_operation = Some(TmuxIoOperationLease {
                phase: TmuxIoOperationPhase::WaitingForCleanExit,
                ..operation
            });
        } else {
            lifecycle.io_operation = None;
        }
        true
    }

    fn try_claim_failure_terminal(
        &self,
        predicate: impl FnOnce(&TmuxLifecycle, State) -> bool,
    ) -> bool {
        let mut lifecycle = self.lifecycle.lock();
        if lifecycle.terminal {
            return false;
        }
        let mut state = self.state.lock();
        if !predicate(&lifecycle, *state) {
            return false;
        }
        lifecycle.terminal = true;
        lifecycle.clean_exit = false;
        lifecycle.io_operation = None;
        lifecycle.detach_disposition = TerminalDetachDisposition::Pending;
        *state = State::Exit;
        true
    }

    fn fail_tmux_io_operation(
        &self,
        kind: TmuxIoOperationKind,
        generation: u64,
        reason: &'static str,
    ) {
        let claimed = self.try_claim_failure_terminal(|lifecycle, _state| {
            lifecycle.io_operation.is_some_and(|operation| {
                operation.kind == kind && operation.generation == generation
            })
        });
        if !claimed {
            metrics::counter!(
                "mux.tmux.io.stale_failure",
                "operation" => kind.label(),
            )
            .increment(1);
            return;
        }
        metrics::counter!(
            "mux.tmux.io.completed",
            "operation" => kind.label(),
            "outcome" => reason,
        )
        .increment(1);
        log::error!(
            "tmux domain {} {} generation {} terminated on its bounded I/O lane: {reason}",
            self.domain_id,
            kind.label(),
            generation
        );
        self.invalidate_launcher_after_io_failure(reason);
        self.publish_terminal_transition(true, false);
    }

    fn fail_initial_guard(&self, reason: &'static str) {
        if !self.try_claim_failure_terminal(|_lifecycle, state| {
            state == State::WaitForInitialGuard
        }) {
            return;
        }
        metrics::counter!(
            "mux.tmux.io.completed",
            "operation" => "initial_guard",
            "outcome" => reason,
        )
        .increment(1);
        log::error!(
            "tmux domain {} failed before its initial guarded boundary: {reason}",
            self.domain_id,
        );
        self.invalidate_launcher_after_io_failure(reason);
        self.publish_terminal_transition(true, false);
    }

    fn fail_io_supervisor(&self, reason: &'static str) {
        if !self.try_claim_failure_terminal(|_lifecycle, _state| true) {
            return;
        }
        metrics::counter!(
            "mux.tmux.io.completed",
            "operation" => "supervisor",
            "outcome" => reason,
        )
        .increment(1);
        log::error!(
            "tmux domain {} lost its I/O supervisor: {reason}",
            self.domain_id,
        );
        self.invalidate_launcher_after_io_failure(reason);
        self.publish_terminal_transition(true, false);
    }

    fn invalidate_launcher_after_io_failure(&self, reason: &str) {
        let Some(mux) = Mux::try_get() else {
            return;
        };
        let Some(domain) = mux.get_domain(self.domain_id) else {
            return;
        };
        let Some(tmux_domain) = domain.downcast_ref::<TmuxDomain>() else {
            return;
        };
        if !std::ptr::eq(tmux_domain.inner.as_ref(), self) {
            return;
        }
        let Some(pane) = mux.get_pane(self.pane_id) else {
            return;
        };
        if let Some(local_pane) = pane.downcast_ref::<LocalPane>() {
            let _ = local_pane.clear_tmux_domain_if(self);
        }
        log::error!(
            "invalidating tmux launcher pane {} for domain {} after {reason}",
            self.pane_id,
            self.domain_id
        );
        pane.kill();
    }

    fn start_io_operation(&self, start: TmuxIoStart) -> Result<(), TmuxIoAdmissionError> {
        self.io_lane
            .get()
            .ok_or(TmuxIoAdmissionError::Unavailable)?
            .start(start)
    }

    fn signal_io_response(&self, generation: u64) -> Result<(), TmuxIoAdmissionError> {
        self.io_lane
            .get()
            .ok_or(TmuxIoAdmissionError::Unavailable)?
            .signal_response(generation)
    }

    fn signal_initial_guard_ready(&self) -> Result<(), TmuxIoAdmissionError> {
        self.io_lane
            .get()
            .ok_or(TmuxIoAdmissionError::Unavailable)?
            .signal_initial_guard_ready()
    }

    #[cfg(test)]
    fn set_test_initial_guard_deadline(
        &self,
        deadline: Duration,
    ) -> Result<(), TmuxIoAdmissionError> {
        self.io_lane
            .get()
            .ok_or(TmuxIoAdmissionError::Unavailable)?
            .set_test_initial_guard_deadline(deadline)
    }

    fn finish_active_operation(&self) {
        let should_finish = {
            let mut lifecycle = self.lifecycle.lock();
            if lifecycle.active_operations == 0 {
                log::error!(
                    "tmux domain {} active-operation counter underflow",
                    self.domain_id
                );
                return;
            }
            lifecycle.active_operations -= 1;
            lifecycle.terminal && lifecycle.active_operations == 0
        };
        if should_finish {
            self.finish_terminal_cleanup_if_ready();
        }
    }

    fn finish_terminal_cleanup_if_ready(&self) {
        {
            let mut lifecycle = self.lifecycle.lock();
            if !lifecycle.terminal || lifecycle.active_operations != 0 {
                return;
            }
            if lifecycle.resources_cleaned {
                drop(lifecycle);
                self.schedule_detach_cleanup();
                return;
            }
            if lifecycle.cleanup_in_progress {
                return;
            }
            lifecycle.cleanup_in_progress = true;
        }

        let remote_panes: Vec<_> = self
            .remote_panes
            .lock()
            .drain()
            .map(|(_, pane)| pane)
            .collect();
        for remote_pane in remote_panes {
            remote_pane
                .lock()
                .child_state
                .mark_exited(portable_pty::ExitStatus::with_exit_code(0));
        }

        // An operation admitted before terminalization can finish allocating a
        // subscription after the immediate unsubscribe in request_terminal.
        // Cleanup runs only after those operations drain, so this second pass
        // closes that publication race.
        self.unsubscribe_notification();
        let config_reload_sub = self.config_reload_sub.lock().take();
        drop(config_reload_sub);
        self.backlog.lock().clear();
        self.retired_panes.lock().clear();
        self.gui_tabs.lock().clear();
        self.mirror_index.lock().clear();
        let pending_splits: Vec<_> = self.pending_splits.lock().drain().collect();
        let _ = self.tmux_session.lock().take();
        self.support_commands.lock().clear();
        *self.attach_state.lock() = AttachState::Init;
        let window_builder = self.gui_window.lock().take();
        if let Some(window_builder) = window_builder {
            window_builder.cancel();
        }

        {
            let mut lifecycle = self.lifecycle.lock();
            lifecycle.cleanup_in_progress = false;
            lifecycle.resources_cleaned = true;
        }
        // Completing a promise can synchronously wake arbitrary executor code.
        // All pending-split map and lifecycle guards must therefore be gone
        // before terminalization publishes the failure.
        for (request_id, mut promise) in pending_splits {
            promise.err(anyhow::anyhow!(
                "tmux split request {request_id} was cancelled because domain {} terminated",
                self.domain_id
            ));
        }
        self.schedule_detach_cleanup();
    }

    fn schedule_detach_cleanup(&self) {
        let ready = {
            let lifecycle = self.lifecycle.lock();
            lifecycle.terminal && lifecycle.resources_cleaned && lifecycle.active_operations == 0
        };
        if !ready {
            return;
        }

        let Some(mux) = Mux::try_get() else {
            return;
        };
        let Some(domain) = mux.get_domain(self.domain_id) else {
            if self.clean_exit_requested.load(Ordering::Acquire) {
                self.clean_detach_completed.store(true, Ordering::Release);
            }
            return;
        };
        let Some(tmux_domain) = domain.downcast_ref::<TmuxDomain>() else {
            log::error!(
                "tmux terminal cleanup found non-tmux domain for id {}; refusing to remove it",
                self.domain_id
            );
            return;
        };
        if !std::ptr::eq(tmux_domain.inner.as_ref(), self) {
            log::error!(
                "tmux terminal cleanup found a replacement domain for id {}; refusing stale removal",
                self.domain_id
            );
            return;
        }

        let expected_inner = Arc::clone(&tmux_domain.inner);
        if mux.is_main_thread() {
            // A background operation may already have queued this cleanup.
            // An explicit main-thread retry must not be blocked behind that
            // pending runnable: removal is idempotent after the exact-instance
            // validation above, and the queued runnable will observe that the
            // domain is gone.
            Self::finalize_terminal_cleanup(&mux, &domain, &expected_inner);
            return;
        }

        if self
            .detach_cleanup_scheduled
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }

        if !promise::spawn::is_scheduler_configured() {
            self.detach_cleanup_scheduled
                .store(false, Ordering::Release);
            log::error!(
                "tmux domain {} is ready for terminal cleanup but no main-thread scheduler is \
                 configured; a later detach request will retry",
                self.domain_id
            );
            return;
        }

        let scheduled = Arc::clone(&self.detach_cleanup_scheduled);
        promise::spawn::spawn_into_main_thread_with_low_priority(async move {
            let mut schedule_lease = DetachScheduleLease {
                scheduled,
                completed: false,
            };
            Self::finalize_terminal_cleanup(&mux, &domain, &expected_inner);
            schedule_lease.completed = true;
        })
        .detach();
    }

    fn finalize_terminal_cleanup(
        mux: &Arc<Mux>,
        domain: &Arc<dyn Domain>,
        expected_inner: &Arc<Self>,
    ) {
        let should_invalidate_launcher = {
            let mut lifecycle = expected_inner.lifecycle.lock();
            if !lifecycle.terminal
                || !lifecycle.resources_cleaned
                || lifecycle.active_operations != 0
                || lifecycle.finalization_in_progress
                || lifecycle.finalized
            {
                return;
            }
            lifecycle.finalization_in_progress = true;
            if lifecycle.detach_disposition == TerminalDetachDisposition::Pending {
                lifecycle.detach_disposition = TerminalDetachDisposition::Claimed;
                true
            } else {
                false
            }
        };

        expected_inner.finalize_launcher_tmux_binding(mux, should_invalidate_launcher);
        let removed = mux.domain_was_detached_if_same(domain);
        let exact_instance_absent = removed
            || mux
                .get_domain(expected_inner.domain_id)
                .is_none_or(|current| !Arc::ptr_eq(&current, domain));

        let clean_exit = {
            let mut lifecycle = expected_inner.lifecycle.lock();
            if lifecycle.detach_disposition == TerminalDetachDisposition::Claimed {
                lifecycle.detach_disposition = TerminalDetachDisposition::Attempted;
            }
            lifecycle.finalization_in_progress = false;
            lifecycle.finalized = exact_instance_absent;
            lifecycle.clean_exit
        };
        if removed && clean_exit {
            expected_inner
                .clean_detach_completed
                .store(true, Ordering::Release);
        }
    }

    fn finalize_launcher_tmux_binding(
        &self,
        mux: &Arc<Mux>,
        should_invalidate_launcher: bool,
    ) {
        let Some(pane) = mux.get_pane(self.pane_id) else {
            log::error!(
                "tmux terminal cleanup cannot find launcher pane {} for domain {}",
                self.pane_id,
                self.domain_id
            );
            return;
        };

        if let Some(local_pane) = pane.downcast_ref::<LocalPane>() {
            let cleared = local_pane.clear_tmux_domain_if(self);
            if !cleared {
                log::warn!(
                    "tmux terminal cleanup found a replacement launcher binding for domain {}",
                    self.domain_id
                );
                return;
            }
        }

        if should_invalidate_launcher {
            // A fail-close path must never perform another potentially
            // blocking launcher write on the GUI/main lane. Invalidating the
            // exact binding and terminating the control-mode client leaves the
            // tmux server/session intact while making a late writer harmless.
            log::warn!(
                "terminating tmux launcher {} after fail-closed domain {} cleanup",
                self.pane_id,
                self.domain_id,
            );
            pane.kill();
        }
    }

    pub(crate) fn is_terminal(&self) -> bool {
        *self.state.lock() == State::Exit
    }

    /// Publishes a newly allocated mux subscription atomically with lifecycle
    /// terminalization. `Ok(false)` means another racing publisher already
    /// installed the required subscription.
    pub(crate) fn publish_notification_subscription(&self, sub_id: usize) -> anyhow::Result<bool> {
        let lifecycle = self.lifecycle.lock();
        if lifecycle.terminal {
            anyhow::bail!(
                "tmux domain {} became terminal before subscription publication",
                self.domain_id
            );
        }
        let mut notification_sub_id = self.notification_sub_id.lock();
        if notification_sub_id.is_some() {
            return Ok(false);
        }
        *notification_sub_id = Some(sub_id);
        Ok(true)
    }

    pub(crate) fn with_active_lifecycle<R>(&self, f: impl FnOnce() -> R) -> Option<R> {
        let _active_operation = self.begin_active_operation()?;
        Some(f())
    }

    /// Performs a compare-and-transition without allowing a stale event to
    /// overwrite a newer protocol state or resurrect an exited domain.
    fn transition_state(&self, expected: State, next: State) -> bool {
        debug_assert_ne!(next, State::Exit);
        let lifecycle = self.lifecycle.lock();
        if lifecycle.terminal {
            return false;
        }
        let mut state = self.state.lock();
        if *state == expected {
            *state = next;
            return true;
        }
        false
    }

    pub(crate) fn resolve_pending_split(&self, request_id: u64, pane_id: TmuxPaneId) -> bool {
        let promise = self.pending_splits.lock().remove(&request_id);
        if let Some(mut promise) = promise {
            promise.ok(pane_id);
            true
        } else {
            false
        }
    }

    pub(crate) fn fail_pending_split(&self, request_id: u64, err: anyhow::Error) -> bool {
        let promise = self.pending_splits.lock().remove(&request_id);
        if let Some(mut promise) = promise {
            promise.err(err);
            true
        } else {
            false
        }
    }

    fn alloc_split_request_id(&self) -> anyhow::Result<u64> {
        self.next_split_request_id
            .try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(1)
            })
            .map_err(|_| anyhow::anyhow!("tmux split request id space exhausted"))
    }

    /// All queue handles, including PTY writers and child killers, share the
    /// closeable/capped `TmuxCmdQueue`; this wrapper preserves the existing
    /// call-site vocabulary while keeping enforcement inside the mailbox.
    fn push_command_capped(
        &self,
        queue: &mut TmuxCmdQueue,
        cmd: Box<dyn TmuxCommand>,
    ) -> Result<(), TmuxEnqueueError> {
        queue.push_back(cmd)
    }

    pub(crate) fn enqueue_required(
        &self,
        command: Box<dyn TmuxCommand>,
        context: &'static str,
    ) -> anyhow::Result<()> {
        self.enqueue_required_batch(vec![command], context)
    }

    pub(crate) fn enqueue_required_batch(
        &self,
        commands: Vec<Box<dyn TmuxCommand>>,
        context: &'static str,
    ) -> anyhow::Result<()> {
        let enqueue_result = {
            let mut queue = self.cmd_queue.lock();
            queue.push_required_batch(commands)
        };
        match enqueue_result {
            Ok(()) => self.require_send_schedule(context),
            Err(err) => {
                let error = anyhow::anyhow!(
                    "required tmux command admission failed for domain {} during {context}: {err}",
                    self.domain_id
                );
                if matches!(
                    err,
                    TmuxEnqueueError::Full | TmuxEnqueueError::ClassMismatch
                ) {
                    self.transition_to_exit_and_schedule_detach();
                }
                Err(error)
            }
        }
    }

    pub fn advance(&self, events: Box<Vec<Event>>) {
        let ingress = self.protocol_ingress.lock();
        let events = *events;
        {
            let mut barrier = self.protocol_barrier.lock();
            if barrier.active {
                let admitted = barrier.enqueue(events).is_ok();
                drop(barrier);
                drop(ingress);
                if !admitted {
                    log::error!(
                        "tmux domain {} exceeded its bounded protocol response barrier; \
                         detaching rather than dropping or reordering events",
                        self.domain_id
                    );
                    self.transition_to_exit_and_schedule_detach();
                }
                return;
            }
        }

        let completed_response = self.process_protocol_events(events);
        drop(ingress);
        if let Some((command, response)) = completed_response {
            self.schedule_command_result(command, response);
        }
    }

    fn process_protocol_events(
        &self,
        events: Vec<Event>,
    ) -> Option<(Box<dyn TmuxCommand>, Guarded)> {
        let mut events = events.into_iter();
        while let Some(event) = events.next() {
            if matches!(&event, Event::Exit { .. }) {
                self.transition_to_clean_exit();
                return None;
            }

            let _active_operation = self.begin_active_operation()?;
            let state = *self.state.lock();
            log::debug!("tmux: {:?} in state {:?}", event, state);
            match &event {
                // Tmux generic events
                Event::Guarded(response) => match state {
                    State::WaitForInitialGuard => {
                        if !self.transition_state(State::WaitForInitialGuard, State::Idle) {
                            return None;
                        }
                        if let Err(err) = self.signal_initial_guard_ready() {
                            log::error!(
                                "tmux domain {} could not cancel its initial-guard deadline: {err}",
                                self.domain_id
                            );
                            self.transition_to_exit_and_schedule_detach();
                            return None;
                        }
                    }
                    State::WaitingForResponse => {
                        let mut cmd_queue = self.cmd_queue.as_ref().lock();
                        if let Some((cmd, resp, generation)) =
                            cmd_queue.record_in_flight_response(response)
                        {
                            let io_kind = if cmd.awaits_clean_exit() {
                                TmuxIoOperationKind::Detach
                            } else {
                                TmuxIoOperationKind::Command
                            };
                            if !self.claim_io_response(io_kind, generation) {
                                drop(cmd_queue);
                                if !self.is_terminal() {
                                    log::error!(
                                        "tmux domain {} could not claim guarded response for \
                                         generation {generation}; detaching to preserve lease \
                                         ownership",
                                        self.domain_id
                                    );
                                    self.transition_to_exit_and_schedule_detach();
                                }
                                return None;
                            }
                            drop(cmd_queue);
                            if let Err(err) = self.signal_io_response(generation) {
                                log::error!(
                                    "tmux domain {} could not cancel response deadline for \
                                     generation {generation}: {err}",
                                    self.domain_id
                                );
                                self.transition_to_exit_and_schedule_detach();
                                return None;
                            }
                            TmuxDomainState::wake_notification_intent_capacity(self.domain_id);
                            // The callback must commit before any event that
                            // follows its Guarded marker, including events
                            // arriving in later parser batches. Activate the
                            // bounded barrier before releasing protocol
                            // ingress so later batches cannot overtake it.
                            let trailing_events: Vec<_> = events.collect();
                            let response_retained_bytes = std::mem::size_of_val(cmd.as_ref())
                                .saturating_add(cmd.mailbox_payload_bytes())
                                .saturating_add(std::mem::size_of::<Guarded>())
                                .saturating_add(resp.output.capacity());
                            let mut barrier = self.protocol_barrier.lock();
                            let admitted = barrier
                                .activate(response_retained_bytes, trailing_events)
                                .is_ok();
                            drop(barrier);
                            if !admitted {
                                log::error!(
                                    "tmux domain {} exceeded its bounded protocol response \
                                     barrier while fencing a command result; detaching",
                                    self.domain_id
                                );
                                self.transition_to_exit_and_schedule_detach();
                                return None;
                            }
                            return Some((cmd, resp));
                        }
                    }
                    State::Idle | State::Sending | State::ProcessingResponse => {
                        log::error!(
                            "tmux domain {} received an unowned guarded response while in \
                             {state:?}; detaching to preserve command/response alignment",
                            self.domain_id
                        );
                        self.transition_to_exit_and_schedule_detach();
                        return None;
                    }
                    State::Exit => {}
                },

                // Tmux specific events
                Event::ConfigError { error } => {
                    // tmux config file error, not our fault, just log it and go
                    log::warn!("tmux configuration error: {error}");
                }
                Event::Exit { .. } => unreachable!("Exit events are handled before lifecycle read"),
                Event::LayoutChange {
                    window,
                    layout,
                    visible_layout: _,
                    raw_flags: _,
                } => {
                    if let Err(err) = self.enqueue_required(
                        Box::new(ListAllPanes {
                            window_id: *window,
                            prune: true,
                            layout_csum: if let Some(l) = layout.get(0..4) {
                                l.to_string()
                            } else {
                                "".to_string()
                            },
                        }),
                        "layout-change pane reconciliation",
                    ) {
                        log::error!("{err:#}");
                        return None;
                    }
                }
                Event::Output { pane, text } => {
                    let remote_pane = {
                        let pane_map = self.remote_panes.lock();
                        if let Some(remote_pane) = pane_map.get(pane) {
                            Some(Arc::clone(remote_pane))
                        } else {
                            let retired_panes = self.retired_panes.lock();
                            if retired_panes.contains(pane) {
                                // Tmux pane ids are lifetime-unique. Output
                                // after retirement is stale protocol data, not
                                // pre-attach data for a future pane.
                                drop(retired_panes);
                                drop(pane_map);
                                log::debug!("discarding late output for retired tmux pane {pane}");
                                continue;
                            }
                            // Keep the map locked through the append. This
                            // closes the absent->insert->late-append race that
                            // could otherwise strand bytes after publication.
                            let _ = self.backlog_limits_dirty.swap(false, Ordering::AcqRel);
                            let limits = TmuxBacklogLimits::current();
                            let mut backlog = self.backlog.lock();
                            backlog.append_with_limits(*pane, text, limits);
                            let recovery_required = backlog.requires_recovery();
                            drop(backlog);
                            drop(retired_panes);
                            drop(pane_map);
                            if recovery_required {
                                log::error!(
                                    "tmux pane {pane} output exceeded the bounded pre-attach \
                                     backlog; detaching rather than replaying a truncated terminal \
                                     stream"
                                );
                                self.transition_to_exit_and_schedule_detach();
                                return None;
                            }
                            log::debug!("Tmux pane {pane} has not been attached");
                            None
                        }
                    };
                    if let Some(ref_pane) = remote_pane {
                        let mut tmux_pane = ref_pane.lock();
                        if self.retired_panes.lock().contains(pane) {
                            // Retirement publishes the tombstone before
                            // removing the registry entry. Recheck after
                            // acquiring a previously cloned pane gate so an
                            // in-flight Output cannot write after retirement.
                            continue;
                        }
                        if tmux_pane.output_state == TmuxPaneOutputState::Ready {
                            if let Err(err) = tmux_pane.output_write.write_all(text) {
                                log::error!("Failed to write tmux data to output: {err:#}");
                                drop(tmux_pane);
                                self.transition_to_exit_and_schedule_detach();
                                return None;
                            }
                        } else if tmux_pane.output_state != TmuxPaneOutputState::Retired {
                            // Pane state serializes this append with the
                            // backlog drain and Fresh/Captured -> Ready commit.
                            let _ = self.backlog_limits_dirty.swap(false, Ordering::AcqRel);
                            let limits = TmuxBacklogLimits::current();
                            let mut backlog = self.backlog.lock();
                            backlog.append_with_limits(*pane, text, limits);
                            let recovery_required = backlog.requires_recovery();
                            drop(backlog);
                            if recovery_required {
                                log::error!(
                                    "tmux pane {pane} output exceeded the bounded preparation \
                                     backlog; detaching rather than replaying a truncated terminal \
                                     stream"
                                );
                                drop(tmux_pane);
                                self.transition_to_exit_and_schedule_detach();
                                return None;
                            }
                        }
                    }
                }
                Event::SessionChanged { session, name: _ } => {
                    if Mux::try_get().is_none() {
                        log::error!(
                            "cannot subscribe tmux domain {} to notifications without an active \
                             mux; detaching the domain",
                            self.domain_id
                        );
                        self.transition_to_exit_and_schedule_detach();
                        return None;
                    }
                    if let Err(err) = self.subscribe_notification() {
                        log::error!(
                            "failed to allocate required tmux mux notification subscription for \
                             domain {}: {err}; detaching the domain",
                            self.domain_id
                        );
                        self.transition_to_exit_and_schedule_detach();
                        return None;
                    }

                    *self.tmux_session.lock() = Some(*session);
                    if let Err(err) = self.enqueue_required(
                        Box::new(ListCommands),
                        "session-change command discovery",
                    ) {
                        log::error!("{err:#}");
                        return None;
                    }

                    log::info!("tmux session changed:{}", session);
                }
                Event::WindowAdd { window } => {
                    // Only handle the new tab, the first empty window handled by sync_window_state
                    if let (true, Some(session)) =
                        (self.gui_window.lock().is_some(), *self.tmux_session.lock())
                    {
                        if let Err(err) = self.enqueue_required(
                            Box::new(ListAllWindows {
                                session_id: session,
                                window_id: Some(*window),
                            }),
                            "window-add topology discovery",
                        ) {
                            log::error!("{err:#}");
                            return None;
                        }
                        log::info!("tmux window add: {}:{}", session, window);
                    }
                }
                Event::WindowClose { window } => {
                    if let Err(err) = self.remove_detached_window(*window) {
                        log::error!(
                            "failed to retire closed tmux window {window} in domain {}: {err:#}",
                            self.domain_id
                        );
                        self.transition_to_exit_and_schedule_detach();
                        return None;
                    }
                }
                Event::WindowPaneChanged { window, pane } => {
                    // The tmux 2.7 WindowPaneChanged event comes early than WindowAdd, we need to
                    // skip it
                    if !self.check_window_attached(*window) {
                        continue;
                    }

                    log::info!("tmux window pane changed: {}:{}", window, pane);
                }
                Event::WindowRenamed { window, name } => {
                    let gui_tabs = self.gui_tabs.lock();
                    if let Some(x) = gui_tabs.get(&window) {
                        if let Some(tab) = Mux::try_get().and_then(|mux| mux.get_tab(x.tab_id)) {
                            tab.set_title(&format!("{}", name));
                        }
                    }
                }
                Event::UnlinkedWindowClose { window } => {
                    if let Err(err) = self.remove_detached_window(*window) {
                        log::error!(
                            "failed to retire unlinked tmux window {window} in domain {}: {err:#}",
                            self.domain_id
                        );
                        self.transition_to_exit_and_schedule_detach();
                        return None;
                    }
                }
                _ => {}
            }
        }

        // send pending commands to tmux
        let should_schedule = {
            let cmd_queue = self.cmd_queue.as_ref().lock();
            *self.state.lock() == State::Idle && cmd_queue.has_pending()
        };
        if should_schedule {
            let _ = self.require_send_schedule("protocol ingress completion");
        }
        None
    }

    fn schedule_command_result(&self, cmd: Box<dyn TmuxCommand>, resp: Guarded) {
        let domain_id = self.domain_id;
        if !promise::spawn::is_scheduler_configured() {
            log::error!(
                "cannot process tmux command result for domain {domain_id}: no scheduler is \
                 configured; detaching the domain"
            );
            self.transition_to_exit_and_schedule_detach();
            return;
        }
        let Some(mux) = Mux::try_get() else {
            self.transition_to_exit_and_schedule_detach();
            return;
        };
        let Some(expected_domain) = mux.get_domain(domain_id) else {
            self.transition_to_exit_and_schedule_detach();
            return;
        };
        let Some(tmux_domain) = expected_domain.downcast_ref::<TmuxDomain>() else {
            self.transition_to_exit_and_schedule_detach();
            return;
        };
        if !std::ptr::eq(tmux_domain.inner.as_ref(), self) {
            self.transition_to_exit_and_schedule_detach();
            return;
        }
        let expected_inner = Arc::clone(&tmux_domain.inner);
        let barrier_lease = ResponseBarrierLease {
            owner: Arc::clone(&expected_inner),
            completed: false,
        };
        promise::spawn::spawn_into_main_thread(async move {
            let mut barrier_lease = barrier_lease;
            let global_matches = Mux::try_get().is_some_and(|current| Arc::ptr_eq(&current, &mux));
            if !global_matches {
                expected_inner.transition_to_exit_and_schedule_detach();
                Self::finalize_terminal_cleanup(&mux, &expected_domain, &expected_inner);
                barrier_lease.completed = true;
                return;
            }
            let Some(current_domain) = mux.get_domain(domain_id) else {
                expected_inner.transition_to_exit_and_schedule_detach();
                barrier_lease.completed = true;
                return;
            };
            if !Arc::ptr_eq(&current_domain, &expected_domain) {
                expected_inner.transition_to_exit_and_schedule_detach();
                barrier_lease.completed = true;
                return;
            }
            expected_inner.complete_command_response(cmd, &resp);
            barrier_lease.completed = true;
        })
        .detach();
    }

    fn apply_command_result(&self, cmd: Box<dyn TmuxCommand>, response: &Guarded) -> bool {
        let Some(_active_operation) = self.begin_active_operation() else {
            return false;
        };
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            cmd.process_result(self.domain_id, response)
        })) {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                log::error!("Tmux processing command result error: {err}");
                self.transition_to_exit_and_schedule_detach();
                return false;
            }
            Err(_) => {
                log::error!(
                    "Tmux command result callback panicked in domain {}; detaching instead of \
                     stranding the response barrier",
                    self.domain_id
                );
                self.transition_to_exit_and_schedule_detach();
                return false;
            }
        }
        true
    }

    fn complete_command_response(self: &Arc<Self>, cmd: Box<dyn TmuxCommand>, response: &Guarded) {
        if self.apply_command_result(cmd, response) {
            self.protocol_barrier.lock().response_committed();
            self.drain_protocol_response_barrier();
        }
    }

    fn schedule_protocol_barrier_drain(self: &Arc<Self>) {
        if !promise::spawn::is_scheduler_configured() {
            self.transition_to_exit_and_schedule_detach();
            return;
        }
        let owner = Arc::clone(self);
        let barrier_lease = ResponseBarrierLease {
            owner: Arc::clone(self),
            completed: false,
        };
        promise::spawn::spawn_into_main_thread(async move {
            let mut barrier_lease = barrier_lease;
            owner.drain_protocol_response_barrier();
            barrier_lease.completed = true;
        })
        .detach();
    }

    fn drain_protocol_response_barrier(self: &Arc<Self>) {
        let mut drained_events = 0usize;
        let mut drained_bytes = 0usize;
        loop {
            if *self.state.lock() == State::Exit {
                let abandoned_protocol_events = { self.protocol_barrier.lock().clear() };
                drop(abandoned_protocol_events);
                return;
            }

            // Bind the popped event outside the `if let` scrutinee. A mutex
            // guard created directly in that scrutinee lives through the
            // entire arm, and fail-closed event handling can re-enter
            // `request_terminal`, which must clear this same barrier.
            let next_event = { self.protocol_barrier.lock().pop_front() };
            if let Some(event) = next_event {
                let event_bytes = TmuxProtocolBarrier::event_retained_bytes(&event);
                if self.process_protocol_events(vec![event]).is_some() {
                    log::error!(
                        "tmux domain {} observed an impossible nested completed response while \
                         draining its protocol barrier; detaching",
                        self.domain_id
                    );
                    self.transition_to_exit_and_schedule_detach();
                    return;
                }
                drained_events = drained_events.saturating_add(1);
                drained_bytes = drained_bytes.saturating_add(event_bytes);
                if (drained_events >= PROTOCOL_BARRIER_DRAIN_EVENT_QUANTUM
                    || drained_bytes >= PROTOCOL_BARRIER_DRAIN_BYTE_QUANTUM)
                    && !self.protocol_barrier.lock().events.is_empty()
                {
                    metrics::counter!("mux.tmux.protocol_barrier.drain_yields").increment(1);
                    self.schedule_protocol_barrier_drain();
                    return;
                }
                continue;
            }

            // Close the empty-check/deactivation race against parser ingress.
            // All ingress takes these locks in the same order.
            let ingress = self.protocol_ingress.lock();
            let mut barrier = self.protocol_barrier.lock();
            if let Some(event) = barrier.pop_front() {
                drop(barrier);
                drop(ingress);
                let event_bytes = TmuxProtocolBarrier::event_retained_bytes(&event);
                if self.process_protocol_events(vec![event]).is_some() {
                    log::error!(
                        "tmux domain {} observed an impossible nested completed response while \
                         releasing its protocol barrier; detaching",
                        self.domain_id
                    );
                    self.transition_to_exit_and_schedule_detach();
                    return;
                }
                drained_events = drained_events.saturating_add(1);
                drained_bytes = drained_bytes.saturating_add(event_bytes);
                if (drained_events >= PROTOCOL_BARRIER_DRAIN_EVENT_QUANTUM
                    || drained_bytes >= PROTOCOL_BARRIER_DRAIN_BYTE_QUANTUM)
                    && !self.protocol_barrier.lock().events.is_empty()
                {
                    metrics::counter!("mux.tmux.protocol_barrier.drain_yields").increment(1);
                    self.schedule_protocol_barrier_drain();
                    return;
                }
                continue;
            }

            if !barrier.active {
                drop(barrier);
                drop(ingress);
                self.transition_to_exit_and_schedule_detach();
                return;
            }
            let drained_protocol_storage = barrier.clear();
            let transitioned = self.transition_state(State::ProcessingResponse, State::Idle);
            drop(barrier);
            drop(ingress);
            // Even an empty VecDeque retains its peak allocation. Free it
            // outside the barrier and ingress critical sections.
            drop(drained_protocol_storage);

            if !transitioned {
                if *self.state.lock() != State::Exit {
                    self.transition_to_exit_and_schedule_detach();
                }
                return;
            }
            let _ = self.require_send_schedule("protocol response barrier drain");
            return;
        }
    }

    #[cfg(test)]
    fn process_command_result(&self, cmd: Box<dyn TmuxCommand>, response: &Guarded) {
        if !self.apply_command_result(cmd, response) {
            return;
        }
        if self.transition_state(State::ProcessingResponse, State::Idle) {
            let _ = self.require_send_schedule("test command-result completion");
        }
    }

    /// send next command at the front of cmd_queue.
    /// must be called inside main thread
    fn send_next_command(self: &Arc<Self>) {
        if let Err(err) = self.send_next_command_inner() {
            log::error!(
                "failed to transmit a tmux command for domain {}: {err:#}; detaching the domain",
                self.domain_id
            );
            self.transition_to_exit_and_schedule_detach();
        }
    }

    fn send_next_command_inner(self: &Arc<Self>) -> anyhow::Result<()> {
        let Some(active_operation) = self.begin_owned_operation() else {
            return Ok(());
        };
        if !self.transition_state(State::Idle, State::Sending) {
            return Ok(());
        }

        let (command, prepared_command) = loop {
            let prepared_command = {
                let mut cmd_queue = self.cmd_queue.as_ref().lock();
                cmd_queue.take_next_for_preparation()
            };
            let Some(prepared_command) = prepared_command else {
                // Keep the queue locked across the Sending -> Idle transition.
                // A producer either enqueues before this transition and is
                // observed here, or enqueues afterward and schedules the new
                // Idle edge itself.
                let cmd_queue = self.cmd_queue.as_ref().lock();
                if cmd_queue.has_pending() {
                    continue;
                }
                if !self.transition_state(State::Sending, State::Idle) {
                    return Ok(());
                }
                return Ok(());
            };

            // Command preparation can inspect mux/tab/pane state and may take
            // auxiliary locks. Do it outside the mailbox critical section so
            // keypress and resize producers never wait behind that work.
            let command = prepared_command.get_command(self.domain_id);
            if !command.is_empty() {
                break (command, prepared_command);
            }
            self.cmd_queue.lock().release_prepared();
            TmuxDomainState::wake_notification_intent_capacity(self.domain_id);
        };

        let io_kind = if prepared_command.awaits_clean_exit() {
            TmuxIoOperationKind::Detach
        } else {
            TmuxIoOperationKind::Command
        };
        let generation = self.alloc_io_generation()?;
        {
            let mut cmd_queue = self.cmd_queue.as_ref().lock();
            if !self.transition_state(State::Sending, State::WaitingForResponse) {
                cmd_queue.release_prepared();
                drop(cmd_queue);
                TmuxDomainState::wake_notification_intent_capacity(self.domain_id);
                return Ok(());
            }
            if !cmd_queue.install_in_flight(prepared_command, generation) {
                anyhow::bail!(
                    "tmux command mailbox closed or already had an in-flight command during sender reservation"
                );
            }
            if !self.install_io_operation(io_kind, generation) {
                anyhow::bail!(
                    "tmux domain {} could not install the unique I/O lease for generation \
                     {generation}",
                    self.domain_id,
                );
            }
        }

        let command_bytes = command.len();
        let start = TmuxIoStart {
            generation,
            kind: io_kind,
            command: Some(command),
            admitted_at: Instant::now(),
            deadlines: self.io_deadlines(),
            _operation: active_operation,
        };
        self.start_io_operation(start).with_context(|| {
            format!(
                "admitting tmux command generation {generation} to the bounded I/O lane"
            )
        })?;
        metrics::counter!(
            "mux.tmux.io.admitted",
            "operation" => io_kind.label(),
        )
        .increment(1);
        metrics::histogram!(
            "mux.tmux.io.command_bytes",
            "operation" => io_kind.label(),
        )
        .record(command_bytes as f64);
        Ok(())
    }

    fn should_schedule_send(&self) -> bool {
        let cmd_queue = self.cmd_queue.lock();
        *self.state.lock() == State::Idle && cmd_queue.has_pending()
    }

    fn try_claim_send_schedule(&self) -> bool {
        self.send_task_scheduled
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub(crate) fn require_send_schedule(&self, context: &str) -> anyhow::Result<()> {
        match self.schedule_send_next_command() {
            Ok(()) => Ok(()),
            Err(err) => {
                log::error!(
                    "tmux domain {} cannot schedule durably admitted work during {context}: \
                     {err}; detaching",
                    self.domain_id
                );
                self.transition_to_exit_and_schedule_detach();
                Err(anyhow::anyhow!(
                    "tmux sender scheduling failed for domain {} during {context}: {err}",
                    self.domain_id
                ))
            }
        }
    }

    /// Edge-trigger one main-thread sender runnable per tmux domain.
    ///
    /// The runnable releases its scheduling lease before checking for more
    /// work. An enqueue racing that boundary therefore either schedules its
    /// own runnable or is observed by the lost-wakeup recheck below.
    pub fn schedule_send_next_command(&self) -> Result<(), TmuxScheduleError> {
        if !promise::spawn::is_scheduler_configured() {
            return Err(TmuxScheduleError::SchedulerUnavailable);
        }
        let mux = Mux::try_get().ok_or(TmuxScheduleError::MuxUnavailable)?;
        let domain = mux
            .get_domain(self.domain_id)
            .ok_or(TmuxScheduleError::DomainUnavailable)?;
        let tmux_domain = domain
            .downcast_ref::<TmuxDomain>()
            .ok_or(TmuxScheduleError::WrongDomainType)?;
        if !std::ptr::eq(tmux_domain.inner.as_ref(), self) {
            return Err(TmuxScheduleError::ReplacedDomain);
        }
        let scheduled_inner = Arc::clone(&tmux_domain.inner);
        if !scheduled_inner.try_claim_send_schedule() {
            return Ok(());
        }
        let schedule_lease = SendScheduleLease {
            owner: Arc::clone(&scheduled_inner),
            completed: false,
        };
        promise::spawn::spawn_into_main_thread(async move {
            let mut schedule_lease = schedule_lease;
            let exact_registration = Mux::try_get()
                .and_then(|mux| mux.get_domain(scheduled_inner.domain_id))
                .and_then(|domain| {
                    domain
                        .downcast_ref::<TmuxDomain>()
                        .map(|tmux_domain| Arc::ptr_eq(&scheduled_inner, &tmux_domain.inner))
                })
                .unwrap_or(false);
            if !exact_registration {
                log::error!(
                    "tmux domain {} lost its exact registration before its sender runnable; \
                     detaching the stale instance",
                    scheduled_inner.domain_id
                );
                scheduled_inner.transition_to_exit_and_schedule_detach();
                schedule_lease.completed = true;
                return;
            }

            scheduled_inner.send_next_command();
            schedule_lease.completed = true;
            drop(schedule_lease);
            if scheduled_inner.should_schedule_send() {
                if let Err(err) = scheduled_inner.schedule_send_next_command() {
                    log::error!(
                        "tmux domain {} lost sender progress while rechecking its mailbox: {err}; \
                         detaching",
                        scheduled_inner.domain_id
                    );
                    scheduled_inner.transition_to_exit_and_schedule_detach();
                }
            }
        })
        .detach();
        Ok(())
    }

    /// create a standalone window for tmux tabs
    pub fn create_gui_window(&self) {
        if self.gui_window.lock().is_none() {
            let Some(mux) = Mux::try_get() else {
                return;
            };
            let window_builder =
                if let Some((_domain, window_id, _tab)) = mux.resolve_pane_id(self.pane_id) {
                    MuxWindowBuilder {
                        window_id,
                        owner: Arc::downgrade(&mux),
                        activity: Some(Activity::new_for_mux(&mux)),
                        provisional: false,
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
        target: &PaneOperationGuard,
        split_request: SplitRequest,
        request_id: u64,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            target.owner().get_domain(self.domain_id).is_some(),
            "tmux split target belongs to a mux without domain {}",
            self.domain_id
        );
        let pane_id = target.pane_id();
        let tmux_pane_id = self
            .mirror_index
            .lock()
            .remote_pane_for_local(pane_id);

        if let Some(id) = tmux_pane_id {
            let enqueued = {
                let mut cmd_queue = self.cmd_queue.as_ref().lock();
                self.push_command_capped(
                    &mut cmd_queue,
                    Box::new(SplitPane {
                        pane_id: id,
                        direction: split_request.direction,
                        request_id,
                    }),
                )
            };
            enqueued.with_context(|| {
                format!("cannot enqueue split for tmux domain {}", self.domain_id)
            })?;
            return Ok(());
        } else {
            anyhow::bail!("Could not find the tmux pane peer for local pane: {pane_id}");
        }
    }
}

impl TmuxDomain {
    pub fn new(pane_id: PaneId) -> anyhow::Result<Self> {
        let domain_id = alloc_domain_id();
        let cmd_queue = TmuxCmdQueue::new();
        let inner = Arc::new(TmuxDomainState {
            domain_id,
            pane_id,
            // parser,
            state: Mutex::new(State::WaitForInitialGuard),
            lifecycle: Mutex::new(TmuxLifecycle::default()),
            clean_exit_requested: Arc::new(AtomicBool::new(false)),
            clean_detach_completed: AtomicBool::new(false),
            detach_cleanup_scheduled: Arc::new(AtomicBool::new(false)),
            send_task_scheduled: Arc::new(AtomicBool::new(false)),
            io_lane: OnceLock::new(),
            next_io_generation: AtomicU64::new(1),
            cmd_queue: Arc::new(Mutex::new(cmd_queue)),
            protocol_ingress: Mutex::new(()),
            protocol_barrier: Mutex::new(TmuxProtocolBarrier::default()),
            gui_window: Mutex::new(None),
            gui_tabs: Mutex::new(HashMap::default()),
            remote_panes: Mutex::new(HashMap::default()),
            mirror_index: Mutex::new(TmuxMirrorIndex::default()),
            notification_intents: Mutex::new(TmuxNotificationIntentState::default()),
            notification_intent_telemetry: TmuxNotificationIntentTelemetry::default(),
            pane_retirement: Mutex::new(()),
            retired_panes: Mutex::new(HashSet::new()),
            tmux_session: Mutex::new(None),
            support_commands: Mutex::new(HashMap::default()),
            attach_state: Mutex::new(AttachState::Init),
            notification_subscription_gate: Mutex::new(()),
            notification_sub_id: Mutex::new(None),
            config_reload_sub: Mutex::new(None),
            backlog_limits_dirty: AtomicBool::new(false),
            pending_splits: Mutex::new(HashMap::default()),
            next_split_request_id: AtomicU64::new(1),
            backlog: Mutex::new(TmuxBacklog::default()),
            #[cfg(test)]
            test_io_deadlines: Mutex::new(None),
        });
        let io_lane = TmuxIoLane::new(domain_id, Arc::downgrade(&inner))
            .with_context(|| format!("cannot start tmux I/O supervisor for domain {domain_id}"))?;
        inner
            .io_lane
            .set(io_lane)
            .unwrap_or_else(|_| unreachable!("tmux I/O lane is initialized exactly once"));
        let weak_inner = Arc::downgrade(&inner);
        let config_reload_sub = config::subscribe_to_config_reload(move || {
            let Some(inner) = weak_inner.upgrade() else {
                return false;
            };
            inner.backlog_limits_dirty.store(true, Ordering::Release);
            if promise::spawn::is_scheduler_configured() {
                let scheduled_inner = Arc::clone(&inner);
                promise::spawn::spawn_into_main_thread(async move {
                    if !scheduled_inner
                        .backlog_limits_dirty
                        .swap(false, Ordering::AcqRel)
                    {
                        return;
                    }
                    // Read the freshly published config before taking the
                    // backlog mutex. Config reload callbacks execute while the
                    // config mutex is held, so the callback itself only marks
                    // this deferred reconciliation.
                    let limits = TmuxBacklogLimits::current();
                    let recovery_required = {
                        let mut backlog = scheduled_inner.backlog.lock();
                        backlog.refresh_limits(limits);
                        backlog.record_metrics();
                        backlog.requires_recovery()
                    };
                    if recovery_required {
                        log::error!(
                            "tmux backlog limits contracted below retained pre-attach output; \
                             detaching rather than replaying a truncated terminal stream"
                        );
                        scheduled_inner.transition_to_exit_and_schedule_detach();
                    }
                })
                .detach();
            }
            true
        });
        *inner.config_reload_sub.lock() = Some(config_reload_sub);

        Ok(Self { inner })
    }

    fn spawn_unsupported(surface: &str) -> anyhow::Error {
        anyhow::anyhow!(
            "{surface} is unsupported for TmuxDomain because tmux control-mode windows and panes \
             materialize asynchronously from tmux events rather than returning an immediate local handle"
        )
    }

}

#[async_trait(?Send)]
impl Domain for TmuxDomain {
    async fn spawn(
        &self,
        _mux: &Arc<Mux>,
        _size: TerminalSize,
        _command: Option<CommandBuilder>,
        _command_dir: Option<String>,
        _window: WindowId,
    ) -> anyhow::Result<Arc<Tab>> {
        Err(Self::spawn_unsupported("spawn"))
    }

    async fn split_pane_spawned(
        &self,
        mux: &Arc<Mux>,
        target: &PaneOperationGuard,
        split_request: SplitRequest,
        _command: Option<CommandBuilder>,
        _command_dir: Option<String>,
    ) -> anyhow::Result<SplitCommitReceipt> {
        anyhow::ensure!(
            target.belongs_to(mux),
            "tmux split target belongs to another mux registration"
        );
        let active_operation = self.inner.begin_active_operation().ok_or_else(|| {
            anyhow::anyhow!(
                "cannot split pane in detached tmux domain {}",
                self.inner.domain_id
            )
        })?;
        let request_id = self.inner.alloc_split_request_id()?;
        let mut promise = promise::Promise::new();
        if let Some(future) = promise.get_future() {
            {
                let mut pending_splits = self.inner.pending_splits.lock();
                self.inner
                    .split_tmux_pane(target, split_request, request_id)?;
                anyhow::ensure!(
                    pending_splits.insert(request_id, promise).is_none(),
                    "duplicate tmux split request id {request_id}"
                );
            }
            self.inner
                .require_send_schedule("split-pane command admission")?;
            drop(active_operation);

            let id = future
                .await
                .context("tmux split command did not produce a pane id")?;
            let _materialize_operation = self.inner.begin_active_operation().ok_or_else(|| {
                anyhow::anyhow!(
                    "tmux domain {} detached before split pane materialization",
                    self.inner.domain_id
                )
            })?;
            return self.inner.split_pane(mux, target, id, split_request);
        }

        anyhow::bail!("Split_pane failed");
    }

    async fn split_pane_moved(
        &self,
        _mux: &Arc<Mux>,
        _target: &PaneOperationGuard,
        _source: &PaneOperationGuard,
        _split_request: SplitRequest,
    ) -> anyhow::Result<SplitCommitReceipt> {
        anyhow::bail!(
            "moving an existing pane into a tmux control-mode split is unsupported"
        )
    }

    async fn spawn_pane(
        &self,
        _mux: &Arc<Mux>,
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

    async fn attach(
        &self,
        _mux: &Arc<Mux>,
        _owner_client_id: Option<Arc<crate::client::ClientId>>,
        _window_id: Option<crate::WindowId>,
    ) -> anyhow::Result<()> {
        // Control-mode startup is bootstrapped by SessionChanged events rather
        // than an explicit attach command.
        Ok(())
    }

    fn detachable(&self) -> bool {
        true
    }

    fn detach(&self) -> anyhow::Result<()> {
        let Some(_detach_admission) = self.inner.begin_owned_operation() else {
            self.inner.finish_terminal_cleanup_if_ready();
            self.inner.schedule_detach_cleanup();
            return Ok(());
        };

        let mux = Mux::try_get()
            .ok_or_else(|| anyhow::anyhow!("cannot detach tmux domain: no mux configured"))?;
        let pane = mux.get_pane(self.inner.pane_id).ok_or_else(|| {
            anyhow::anyhow!(
                "detach is unavailable for TmuxDomain because its launcher pane {} is gone",
                self.inner.pane_id
            )
        })?;
        let registered_domain = mux.get_domain(self.inner.domain_id).ok_or_else(|| {
            anyhow::anyhow!(
                "cannot detach tmux domain {} because it is not registered",
                self.inner.domain_id
            )
        })?;
        let registered_tmux = registered_domain
            .downcast_ref::<TmuxDomain>()
            .context("registered detach target is not a tmux domain")?;
        anyhow::ensure!(
            Arc::ptr_eq(&registered_tmux.inner, &self.inner),
            "registered tmux detach target is a replacement instance"
        );
        drop(pane);

        anyhow::ensure!(
            !self.inner.clean_exit_requested.load(Ordering::Acquire),
            "cannot enqueue detach after tmux clean exit was requested"
        );

        let enqueue_result = {
            let mut queue = self.inner.cmd_queue.lock();
            if queue.has_domain_detach_pending() {
                return Ok(());
            }
            queue.push_domain_detach(Box::new(DetachClient))
        };
        if let Err(err) = enqueue_result {
            if err == TmuxEnqueueError::Closed && self.inner.is_terminal() {
                return Ok(());
            }
            self.inner.transition_to_exit_and_schedule_detach();
            return Err(anyhow::anyhow!(
                "cannot admit serialized detach for tmux domain {}: {err}",
                self.inner.domain_id
            ));
        }
        self.inner
            .require_send_schedule("explicit tmux detach admission")
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
    use crate::tab::SplitDirection;
    use crate::tmux_commands::{
        Resize, SendKeys, SplitPane, TmuxCommand, TmuxCommandClass,
    };
    use frankenterm_term::color::ColorPalette;
    use frankenterm_term::{KeyCode, KeyModifiers};
    use parking_lot::{MappedMutexGuard, MutexGuard};
    use promise::spawn::{block_on, ScopedExecutor};
    use rangeset::RangeSet;
    use std::ops::Range;
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc::{self, Receiver, SyncSender};
    use std::sync::{Arc, Mutex as StdMutex, MutexGuard as StdMutexGuard};
    use std::time::Duration;
    use termwiz::surface::{Line, SEQ_ZERO};
    use url::Url;

    fn mux_test_lock() -> &'static StdMutex<()> {
        &crate::MUX_TEST_LOCK
    }

    fn new_tmux_domain(pane_id: PaneId) -> TmuxDomain {
        TmuxDomain::new(pane_id).expect("start tmux test domain I/O supervisor")
    }

    #[test]
    fn tmux_io_lane_spawn_failure_is_propagated() {
        let (control, _receiver) = bounded(TMUX_IO_CONTROL_CAPACITY);
        let spawn_result: std::io::Result<std::thread::JoinHandle<()>> =
            Err(std::io::Error::other("injected supervisor spawn failure"));

        let result = TmuxIoLane::from_spawn_result(42, control, spawn_result);
        let Err(err) = result else {
            panic!("injected tmux I/O supervisor spawn failure must be rejected");
        };
        assert_eq!(err.kind(), std::io::ErrorKind::Other);
        assert_eq!(err.to_string(), "injected supervisor spawn failure");
    }

    #[test]
    fn tmux_initial_guard_commit_fences_a_late_deadline_claim() {
        let tmux_domain = new_tmux_domain(180);
        assert!(tmux_domain
            .inner
            .transition_state(State::WaitForInitialGuard, State::Idle));

        tmux_domain.inner.fail_initial_guard("test_late_deadline");

        assert_eq!(*tmux_domain.inner.state.lock(), State::Idle);
        assert!(!tmux_domain.inner.lifecycle.lock().terminal);
    }

    #[test]
    fn tmux_guarded_response_claim_fences_a_late_command_deadline() {
        let tmux_domain = new_tmux_domain(181);
        *tmux_domain.inner.state.lock() = State::WaitingForResponse;
        assert!(tmux_domain
            .inner
            .install_io_operation(TmuxIoOperationKind::Command, 41));
        assert!(tmux_domain
            .inner
            .claim_io_response(TmuxIoOperationKind::Command, 41));

        tmux_domain.inner.fail_tmux_io_operation(
            TmuxIoOperationKind::Command,
            41,
            "test_late_deadline",
        );

        assert_eq!(
            *tmux_domain.inner.state.lock(),
            State::ProcessingResponse
        );
        let lifecycle = tmux_domain.inner.lifecycle.lock();
        assert!(!lifecycle.terminal);
        assert!(lifecycle.io_operation.is_none());
    }

    #[test]
    fn tmux_detach_deadline_and_clean_exit_have_first_claim_authority() {
        let clean_winner = new_tmux_domain(182);
        *clean_winner.inner.state.lock() = State::WaitingForResponse;
        assert!(clean_winner
            .inner
            .install_io_operation(TmuxIoOperationKind::Detach, 51));
        assert!(clean_winner
            .inner
            .claim_io_response(TmuxIoOperationKind::Detach, 51));
        clean_winner.inner.transition_to_clean_exit();
        clean_winner.inner.fail_tmux_io_operation(
            TmuxIoOperationKind::Detach,
            51,
            "test_late_deadline",
        );
        {
            let lifecycle = clean_winner.inner.lifecycle.lock();
            assert!(lifecycle.terminal);
            assert!(lifecycle.clean_exit);
            assert!(lifecycle.io_operation.is_none());
        }

        let deadline_winner = new_tmux_domain(183);
        *deadline_winner.inner.state.lock() = State::WaitingForResponse;
        assert!(deadline_winner
            .inner
            .install_io_operation(TmuxIoOperationKind::Detach, 52));
        deadline_winner.inner.fail_tmux_io_operation(
            TmuxIoOperationKind::Detach,
            52,
            "test_deadline",
        );
        deadline_winner.inner.transition_to_clean_exit();
        let lifecycle = deadline_winner.inner.lifecycle.lock();
        assert!(lifecycle.terminal);
        assert!(!lifecycle.clean_exit);
        assert!(lifecycle.io_operation.is_none());
        assert!(!deadline_winner
            .inner
            .clean_exit_requested
            .load(Ordering::Acquire));
    }

    fn wait_until(label: &str, mut predicate: impl FnMut() -> bool) {
        let deadline = Instant::now()
            .checked_add(Duration::from_secs(5))
            .expect("test deadline should fit Instant");
        while !predicate() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {}",
                label
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    struct ScopedMux {
        prior: Option<Arc<Mux>>,
        _executor: ScopedExecutor,
        _guard: StdMutexGuard<'static, ()>,
    }

    impl ScopedMux {
        fn install(mux: Arc<Mux>) -> Self {
            let guard = mux_test_lock()
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

        fn shutdown() -> Self {
            let guard = mux_test_lock()
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            let executor = ScopedExecutor::new();
            let prior = Mux::try_get();
            Mux::shutdown();
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

    struct RecordingPane {
        pane_id: PaneId,
        domain_id: DomainId,
        keys: Mutex<Vec<char>>,
        writes: Mutex<RecordingWriter>,
        mux_registration: Arc<crate::PaneRegistrationSlot>,
    }

    struct RecordingWriter {
        bytes: Vec<u8>,
        write_threads: Vec<std::thread::ThreadId>,
        write_gate: Option<WriteGate>,
        write_error: Option<std::io::ErrorKind>,
    }

    struct WriteGate {
        entered: Option<SyncSender<()>>,
        release: Receiver<()>,
    }

    #[derive(Debug)]
    struct CountingCommand {
        processed: Arc<AtomicUsize>,
    }

    #[derive(Debug)]
    struct AssertSessionUnsetCommand {
        owner: Weak<TmuxDomainState>,
        processed: Arc<AtomicBool>,
    }

    #[derive(Debug)]
    struct TerminalizingPreparationCommand {
        owner: Weak<TmuxDomainState>,
    }

    #[derive(Debug)]
    struct ClassedTestCommand {
        class: TmuxCommandClass,
        sequence: usize,
    }

    impl TmuxCommand for ClassedTestCommand {
        fn mailbox_class(&self) -> TmuxCommandClass {
            self.class
        }

        fn get_command(&self, _domain_id: DomainId) -> String {
            format!("test-{}\n", self.sequence)
        }

        fn process_result(
            &self,
            _domain_id: DomainId,
            _result: &Guarded,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn complete_next_mailbox_command(queue: &mut TmuxCmdQueue) -> Box<dyn TmuxCommand> {
        let command = queue
            .take_next_for_preparation()
            .expect("mailbox command should be ready");
        assert!(queue.install_in_flight(command, 1));
        let response = Guarded {
            error: false,
            timestamp: 0,
            number: 0,
            flags: 0,
            output: String::new(),
        };
        queue
            .record_in_flight_response(&response)
            .expect("single-response test command should complete")
            .0
    }

    impl TmuxCommand for AssertSessionUnsetCommand {
        fn mailbox_class(&self) -> TmuxCommandClass {
            TmuxCommandClass::RequiredControl
        }

        fn get_command(&self, _domain_id: DomainId) -> String {
            "assert-session-order\n".to_string()
        }

        fn process_result(&self, _domain_id: DomainId, _result: &Guarded) -> anyhow::Result<()> {
            let owner = self
                .owner
                .upgrade()
                .context("tmux test domain disappeared")?;
            anyhow::ensure!(
                owner.tmux_session.lock().is_none(),
                "protocol tail overtook its guarded response callback"
            );
            self.processed.store(true, Ordering::Release);
            Ok(())
        }
    }

    impl TmuxCommand for TerminalizingPreparationCommand {
        fn mailbox_class(&self) -> TmuxCommandClass {
            TmuxCommandClass::RequiredControl
        }

        fn get_command(&self, _domain_id: DomainId) -> String {
            self.owner
                .upgrade()
                .expect("test tmux domain should remain alive")
                .transition_to_exit_and_schedule_detach();
            "terminalized-during-preparation\n".to_string()
        }

        fn process_result(
            &self,
            _domain_id: DomainId,
            _result: &Guarded,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    impl TmuxCommand for CountingCommand {
        fn mailbox_class(&self) -> TmuxCommandClass {
            TmuxCommandClass::RequiredControl
        }

        fn get_command(&self, _domain_id: DomainId) -> String {
            "count\n".to_string()
        }

        fn process_result(&self, _domain_id: DomainId, _result: &Guarded) -> anyhow::Result<()> {
            self.processed.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    impl Write for RecordingWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.write_threads.push(std::thread::current().id());
            if let Some(mut gate) = self.write_gate.take() {
                if let Some(entered) = gate.entered.take() {
                    entered.send(()).map_err(|_| {
                        std::io::Error::new(
                            std::io::ErrorKind::BrokenPipe,
                            "tmux test write-entry receiver dropped",
                        )
                    })?;
                }
                gate.release.recv().map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "tmux test write-release sender dropped",
                    )
                })?;
            }
            if let Some(kind) = self.write_error.take() {
                return Err(std::io::Error::new(
                    kind,
                    "intentional tmux test writer failure",
                ));
            }
            self.bytes.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl RecordingPane {
        fn new(pane_id: PaneId, domain_id: DomainId) -> Arc<Self> {
            Arc::new(Self {
                pane_id,
                domain_id,
                keys: Mutex::new(Vec::new()),
                writes: Mutex::new(RecordingWriter {
                    bytes: Vec::new(),
                    write_threads: Vec::new(),
                    write_gate: None,
                    write_error: None,
                }),
                mux_registration: Arc::new(crate::PaneRegistrationSlot::default()),
            })
        }

        fn new_with_blocking_writer(
            pane_id: PaneId,
            domain_id: DomainId,
        ) -> (Arc<Self>, Receiver<()>, SyncSender<()>) {
            let (entered_tx, entered_rx) = mpsc::sync_channel(1);
            let (release_tx, release_rx) = mpsc::sync_channel(1);
            let pane = Arc::new(Self {
                pane_id,
                domain_id,
                keys: Mutex::new(Vec::new()),
                writes: Mutex::new(RecordingWriter {
                    bytes: Vec::new(),
                    write_threads: Vec::new(),
                    write_gate: Some(WriteGate {
                        entered: Some(entered_tx),
                        release: release_rx,
                    }),
                    write_error: None,
                }),
                mux_registration: Arc::new(crate::PaneRegistrationSlot::default()),
            });
            (pane, entered_rx, release_tx)
        }

        fn new_with_failing_writer(pane_id: PaneId, domain_id: DomainId) -> Arc<Self> {
            Arc::new(Self {
                pane_id,
                domain_id,
                keys: Mutex::new(Vec::new()),
                writes: Mutex::new(RecordingWriter {
                    bytes: Vec::new(),
                    write_threads: Vec::new(),
                    write_gate: None,
                    write_error: Some(std::io::ErrorKind::BrokenPipe),
                }),
                mux_registration: Arc::new(crate::PaneRegistrationSlot::default()),
            })
        }

        fn recorded_keys(&self) -> Vec<char> {
            self.keys.lock().clone()
        }

        fn recorded_writes(&self) -> Vec<u8> {
            self.writes.lock().bytes.clone()
        }

        fn recorded_write_threads(&self) -> Vec<std::thread::ThreadId> {
            self.writes.lock().write_threads.clone()
        }
    }

    impl Pane for RecordingPane {
        fn pane_id(&self) -> PaneId {
            self.pane_id
        }

        fn mux_registration_slot(&self) -> &Arc<crate::PaneRegistrationSlot> {
            &self.mux_registration
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
    fn tmux_backlog_keeps_small_fragmented_payload_in_order() {
        let mut backlog = TmuxBacklog::default();
        let limits = TmuxBacklogLimits::new(32, 64, 4);
        backlog.append_with_limits(1, b"hello ", limits);
        backlog.append_with_limits(1, b"world", limits);

        assert_eq!(backlog.pane_bytes(1), Some(b"hello world".to_vec()));
        assert_eq!(backlog.total_bytes(), 11);
    }

    #[test]
    fn tmux_domain_detach_serializes_one_control_command_and_clean_exit() {
        let default_domain: Arc<dyn Domain> =
            Arc::new(LocalDomain::new("tmux-detach-default").expect("local domain"));
        let mux = Arc::new(Mux::new(Some(Arc::clone(&default_domain))));
        let _guard = ScopedMux::install(Arc::clone(&mux));

        let launcher = RecordingPane::new(77, default_domain.domain_id());
        let launcher_dyn: Arc<dyn Pane> = launcher.clone();
        mux.add_pane(&launcher_dyn).expect("add launcher pane");

        let tmux_domain = Arc::new(new_tmux_domain(77));
        let registered: Arc<dyn Domain> = tmux_domain.clone();
        mux.add_domain(&registered)
            .expect("register tmux detach test domain");
        assert!(tmux_domain.detachable());
        *tmux_domain.inner.state.lock() = State::Idle;
        tmux_domain.detach().expect("detach tmux domain");
        tmux_domain
            .detach()
            .expect("duplicate detach request is idempotent");
        // `ScopedExecutor` deliberately does not pump detached main-thread
        // tasks in the background. Drive the sender edge directly while
        // retaining the production admission and single-flight assertions.
        tmux_domain.inner.send_next_command();

        wait_until("off-main serialized tmux detach write", || {
            launcher.recorded_writes() == b"detach\n"
        });
        assert_eq!(launcher.recorded_writes(), b"detach\n".to_vec());
        assert!(launcher.recorded_keys().is_empty());

        let completed = tmux_domain
            .inner
            .process_protocol_events(vec![
                Event::Guarded(successful_guarded_response()),
                Event::Exit { reason: None },
            ])
            .expect("detach Guarded must own its clean-exit protocol tail");
        tmux_domain
            .inner
            .complete_command_response(completed.0, &completed.1);
        wait_until("serialized tmux detach clean exit", || {
            *tmux_domain.inner.state.lock() == State::Exit
        });
        assert_eq!(
            launcher.recorded_writes(),
            b"detach\n".to_vec(),
            "duplicate detach requests must share the terminal barrier"
        );
    }

    #[test]
    fn tmux_domain_detach_waits_for_in_flight_command_response() {
        let default_domain: Arc<dyn Domain> =
            Arc::new(LocalDomain::new("tmux-detach-order-default").expect("local domain"));
        let mux = Arc::new(Mux::new(Some(Arc::clone(&default_domain))));
        let _guard = ScopedMux::install(Arc::clone(&mux));

        let launcher = RecordingPane::new(78, default_domain.domain_id());
        let launcher_dyn: Arc<dyn Pane> = launcher.clone();
        mux.add_pane(&launcher_dyn).expect("add launcher pane");

        let tmux_domain = Arc::new(new_tmux_domain(78));
        let registered: Arc<dyn Domain> = tmux_domain.clone();
        mux.add_domain(&registered)
            .expect("register ordered tmux detach test domain");
        *tmux_domain.inner.state.lock() = State::Idle;

        let processed = Arc::new(AtomicUsize::new(0));
        tmux_domain
            .inner
            .cmd_queue
            .lock()
            .push_back(Box::new(CountingCommand {
                processed: Arc::clone(&processed),
            }))
            .expect("admit command ahead of detach");
        tmux_domain.inner.send_next_command();
        wait_until("ordinary command write before detach", || {
            launcher.recorded_writes() == b"count\n"
        });

        tmux_domain
            .detach()
            .expect("admit detach behind in-flight command");
        assert_eq!(
            launcher.recorded_writes(),
            b"count\n".to_vec(),
            "detach must not overlap the response lease of the in-flight command"
        );

        let completed = tmux_domain
            .inner
            .process_protocol_events(vec![Event::Guarded(successful_guarded_response())])
            .expect("ordinary command response must retain exact ownership");
        tmux_domain
            .inner
            .complete_command_response(completed.0, &completed.1);
        tmux_domain.inner.send_next_command();
        wait_until("detach write after prior response", || {
            launcher.recorded_writes() == b"count\ndetach\n"
        });
        assert_eq!(processed.load(Ordering::Acquire), 1);

        let completed = tmux_domain
            .inner
            .process_protocol_events(vec![
                Event::Guarded(successful_guarded_response()),
                Event::Exit { reason: None },
            ])
            .expect("detach response must fence its clean-exit tail");
        tmux_domain
            .inner
            .complete_command_response(completed.0, &completed.1);
        wait_until("ordered tmux detach clean exit", || {
            *tmux_domain.inner.state.lock() == State::Exit
        });
        assert_eq!(
            launcher.recorded_writes(),
            b"count\ndetach\n".to_vec()
        );
    }

    #[test]
    fn tmux_domain_detach_requires_launcher_pane() {
        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(mux);

        let tmux_domain = new_tmux_domain(1234);
        let err = tmux_domain
            .detach()
            .expect_err("detach should fail without launcher pane");
        let err = err.to_string();
        assert!(err.contains("launcher pane"), "{}", err);
        assert!(err.contains("TmuxDomain"), "{}", err);
    }

    #[test]
    fn tmux_domain_spawn_is_explicitly_unsupported_without_queueing_side_effects() {
        let mux = Arc::new(Mux::new(None));
        let tmux_domain = new_tmux_domain(77);
        let err = match block_on(tmux_domain.spawn(&mux, TerminalSize::default(), None, None, 0)) {
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
        let mux = Arc::new(Mux::new(None));
        let tmux_domain = new_tmux_domain(77);
        let err = match block_on(tmux_domain.spawn_pane(&mux, TerminalSize::default(), None, None))
        {
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
    fn tmux_exit_during_in_flight_write_is_nonblocking_and_remains_terminal() {
        let default_domain: Arc<dyn Domain> =
            Arc::new(LocalDomain::new("tmux-send-exit-default").expect("local domain"));
        let mux = Arc::new(Mux::new(Some(Arc::clone(&default_domain))));
        let _guard = ScopedMux::install(Arc::clone(&mux));

        let (launcher, write_entered, release_write) =
            RecordingPane::new_with_blocking_writer(89, default_domain.domain_id());
        let launcher_dyn: Arc<dyn Pane> = launcher.clone();
        mux.add_pane(&launcher_dyn).expect("add launcher pane");

        let tmux_domain = Arc::new(new_tmux_domain(89));
        let domain: Arc<dyn Domain> = tmux_domain.clone();
        mux.add_domain(&domain)
            .expect("register tmux blocked-write test domain");
        let inner = Arc::clone(&tmux_domain.inner);
        *inner.state.lock() = State::Idle;
        assert!(
            inner
                .cmd_queue
                .lock()
                .push_back(Box::new(ListCommands))
                .is_ok(),
            "live tmux mailbox must accept the test command"
        );

        let caller_thread = std::thread::current().id();
        inner.send_next_command();
        if let Err(err) = write_entered.recv_timeout(Duration::from_secs(5)) {
            let _ = release_write.send(());
            panic!("tmux command did not reach the blocking writer: {}", err);
        }
        assert_eq!(*inner.state.lock(), State::WaitingForResponse);

        let (exit_finished_tx, exit_finished_rx) = mpsc::sync_channel(1);
        let exit_inner = Arc::clone(&inner);
        let exit_thread = std::thread::spawn(move || {
            exit_inner.transition_to_exit_and_schedule_detach();
            exit_finished_tx
                .send(())
                .expect("announce completed exit transition");
        });
        exit_finished_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("terminal transition must not wait on external pane I/O");
        assert_eq!(*inner.state.lock(), State::Exit);
        {
            let lifecycle = inner.lifecycle.lock();
            assert!(lifecycle.terminal);
            assert_eq!(lifecycle.active_operations, 1);
            assert!(
                !lifecycle.resources_cleaned,
                "terminal cleanup must wait for the admitted launcher write"
            );
        }
        {
            let queue = inner.cmd_queue.lock();
            assert!(
                queue.is_closed(),
                "terminal transition must close the queue"
            );
            assert!(queue.is_empty(), "terminal transition must clear the queue");
        }

        release_write
            .send(())
            .expect("release blocked tmux command write");
        exit_thread.join().expect("exit thread should finish");

        wait_until("terminal cleanup after admitted tmux write", || {
            inner.lifecycle.lock().active_operations == 0
        });
        assert_eq!(*inner.state.lock(), State::Exit);
        {
            let lifecycle = inner.lifecycle.lock();
            assert_eq!(lifecycle.active_operations, 0);
            assert!(
                lifecycle.resources_cleaned,
                "cleanup must complete after the admitted launcher write drains"
            );
        }
        assert!(
            !launcher.recorded_writes().is_empty(),
            "a command reserved before Exit may finish, but must not resurrect the domain"
        );
        assert!(
            launcher
                .recorded_write_threads()
                .iter()
                .all(|writer| writer != &caller_thread),
            "launcher writer acquisition and write must stay off the caller/main lane"
        );
    }

    #[test]
    fn tmux_clean_exit_during_in_flight_write_defers_removal_without_detach_sequence() {
        let default_domain: Arc<dyn Domain> =
            Arc::new(LocalDomain::new("tmux-clean-send-exit-default").expect("local domain"));
        let mux = Arc::new(Mux::new(Some(Arc::clone(&default_domain))));
        let _guard = ScopedMux::install(Arc::clone(&mux));

        let (launcher, write_entered, release_write) =
            RecordingPane::new_with_blocking_writer(98, default_domain.domain_id());
        let launcher_dyn: Arc<dyn Pane> = launcher.clone();
        mux.add_pane(&launcher_dyn).expect("add launcher pane");

        let tmux_domain = Arc::new(new_tmux_domain(98));
        let domain: Arc<dyn Domain> = tmux_domain.clone();
        mux.add_domain(&domain)
            .expect("test tmux domain should register");
        let inner = Arc::clone(&tmux_domain.inner);
        *inner.state.lock() = State::Idle;
        assert!(inner
            .cmd_queue
            .lock()
            .push_back(Box::new(ListCommands))
            .is_ok());

        inner.send_next_command();
        if let Err(err) = write_entered.recv_timeout(Duration::from_secs(5)) {
            let _ = release_write.send(());
            panic!("tmux command did not reach the blocking writer: {}", err);
        }

        let (exit_finished_tx, exit_finished_rx) = mpsc::sync_channel(1);
        let exit_inner = Arc::clone(&inner);
        let exit_thread = std::thread::spawn(move || {
            exit_inner.transition_to_clean_exit();
            exit_finished_tx
                .send(())
                .expect("announce completed clean exit transition");
        });
        exit_finished_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("clean terminal transition must not wait on external pane I/O");

        assert_eq!(*inner.state.lock(), State::Exit);
        assert!(
            mux.get_domain(inner.domain_id).is_some(),
            "clean removal must wait for the admitted launcher write"
        );
        {
            let lifecycle = inner.lifecycle.lock();
            assert!(lifecycle.clean_exit);
            assert_eq!(lifecycle.active_operations, 1);
            assert!(!lifecycle.resources_cleaned);
        }

        release_write
            .send(())
            .expect("release blocked tmux command write");
        exit_thread.join().expect("exit thread should finish");

        wait_until("clean tmux cleanup after admitted write", || {
            let lifecycle = inner.lifecycle.lock();
            lifecycle.active_operations == 0 && lifecycle.resources_cleaned
        });
        tmux_domain
            .detach()
            .expect("main-thread clean-removal retry");
        assert!(mux.get_domain(inner.domain_id).is_none());
        assert_eq!(
            launcher.recorded_writes(),
            b"list-commands\n".to_vec(),
            "clean exit must not append a fail-close detach sequence"
        );
        let lifecycle = inner.lifecycle.lock();
        assert_eq!(lifecycle.active_operations, 0);
        assert!(lifecycle.resources_cleaned);
    }

    #[test]
    fn tmux_command_send_fails_closed_when_launcher_pane_disappears() {
        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(Arc::clone(&mux));
        let tmux_domain = Arc::new(new_tmux_domain(90));
        let domain: Arc<dyn Domain> = tmux_domain.clone();
        mux.add_domain(&domain)
            .expect("register missing-launcher tmux domain");
        *tmux_domain.inner.state.lock() = State::Idle;
        assert!(tmux_domain
            .inner
            .cmd_queue
            .lock()
            .push_back(Box::new(ListCommands))
            .is_ok());

        tmux_domain.inner.send_next_command();

        wait_until("missing launcher pane terminal outcome", || {
            *tmux_domain.inner.state.lock() == State::Exit
        });
        assert_eq!(*tmux_domain.inner.state.lock(), State::Exit);
        assert!(
            tmux_domain.inner.cmd_queue.lock().is_empty(),
            "terminal cleanup must clear an unsendable command"
        );
    }

    #[test]
    fn tmux_command_send_fails_closed_when_mux_disappears() {
        let _guard = ScopedMux::shutdown();
        let tmux_domain = new_tmux_domain(91);
        *tmux_domain.inner.state.lock() = State::Idle;
        assert!(tmux_domain
            .inner
            .cmd_queue
            .lock()
            .push_back(Box::new(ListCommands))
            .is_ok());

        tmux_domain.inner.send_next_command();

        wait_until("missing mux terminal outcome", || {
            *tmux_domain.inner.state.lock() == State::Exit
        });
        assert_eq!(*tmux_domain.inner.state.lock(), State::Exit);
        let queue = tmux_domain.inner.cmd_queue.lock();
        assert!(queue.is_closed());
        assert!(queue.is_empty());
    }

    #[test]
    fn tmux_command_send_fails_closed_on_launcher_write_error() {
        let default_domain: Arc<dyn Domain> =
            Arc::new(LocalDomain::new("tmux-write-error-default").expect("local domain"));
        let mux = Arc::new(Mux::new(Some(Arc::clone(&default_domain))));
        let _guard = ScopedMux::install(Arc::clone(&mux));

        let launcher = RecordingPane::new_with_failing_writer(92, default_domain.domain_id());
        let launcher_dyn: Arc<dyn Pane> = launcher;
        mux.add_pane(&launcher_dyn).expect("add launcher pane");

        let tmux_domain = Arc::new(new_tmux_domain(92));
        let domain: Arc<dyn Domain> = tmux_domain.clone();
        mux.add_domain(&domain)
            .expect("register failing-writer tmux domain");
        *tmux_domain.inner.state.lock() = State::Idle;
        assert!(tmux_domain
            .inner
            .cmd_queue
            .lock()
            .push_back(Box::new(ListCommands))
            .is_ok());

        tmux_domain.inner.send_next_command();

        wait_until("launcher write failure terminal outcome", || {
            *tmux_domain.inner.state.lock() == State::Exit
        });
        assert_eq!(*tmux_domain.inner.state.lock(), State::Exit);
        let queue = tmux_domain.inner.cmd_queue.lock();
        assert!(queue.is_closed());
        assert!(queue.is_empty());
    }

    #[test]
    fn tmux_initial_guard_deadline_terminalizes_silent_startup() {
        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(Arc::clone(&mux));
        let tmux_domain = Arc::new(new_tmux_domain(119));
        let domain: Arc<dyn Domain> = tmux_domain.clone();
        mux.add_domain(&domain)
            .expect("register initial-guard timeout tmux domain");
        tmux_domain
            .inner
            .set_test_initial_guard_deadline(Duration::from_millis(25))
            .expect("shorten initial-guard test deadline");

        wait_until("silent tmux initial-guard terminal outcome", || {
            let lifecycle = tmux_domain.inner.lifecycle.lock();
            lifecycle.terminal && lifecycle.resources_cleaned
        });
        assert_eq!(*tmux_domain.inner.state.lock(), State::Exit);
        let lifecycle = tmux_domain.inner.lifecycle.lock();
        assert!(lifecycle.terminal);
        assert!(lifecycle.resources_cleaned);
        drop(lifecycle);
        assert!(tmux_domain.inner.cmd_queue.lock().is_closed());
    }

    #[test]
    fn tmux_terminalization_during_command_preparation_cannot_strand_sending() {
        let tmux_domain = new_tmux_domain(118);
        *tmux_domain.inner.state.lock() = State::Idle;
        tmux_domain
            .inner
            .cmd_queue
            .lock()
            .push_back(Box::new(TerminalizingPreparationCommand {
                owner: Arc::downgrade(&tmux_domain.inner),
            }))
            .expect("preparation-cancellation command admission");

        tmux_domain.inner.send_next_command();

        assert_eq!(*tmux_domain.inner.state.lock(), State::Exit);
        let lifecycle = tmux_domain.inner.lifecycle.lock();
        assert_eq!(lifecycle.active_operations, 0);
        assert!(lifecycle.resources_cleaned);
        drop(lifecycle);
        let queue = tmux_domain.inner.cmd_queue.lock();
        assert!(queue.is_closed());
        assert!(queue.is_empty());
    }

    #[test]
    fn dropped_tmux_result_dispatch_lease_terminalizes_protocol_barrier() {
        let tmux_domain = new_tmux_domain(117);
        *tmux_domain.inner.state.lock() = State::ProcessingResponse;
        tmux_domain
            .inner
            .protocol_barrier
            .lock()
            .activate(128, vec![Event::SessionsChanged])
            .expect("activate test response barrier");
        let lost = ResponseBarrierLease {
            owner: Arc::clone(&tmux_domain.inner),
            completed: false,
        };

        drop(lost);

        assert_eq!(*tmux_domain.inner.state.lock(), State::Exit);
        let barrier = tmux_domain.inner.protocol_barrier.lock();
        assert!(!barrier.active);
        assert!(barrier.events.is_empty());
        assert_eq!(barrier.retained_bytes, 0);
    }

    #[test]
    fn tmux_command_write_timeout_is_terminal_without_blocking_caller() {
        let default_domain: Arc<dyn Domain> =
            Arc::new(LocalDomain::new("tmux-write-timeout-default").expect("local domain"));
        let mux = Arc::new(Mux::new(Some(Arc::clone(&default_domain))));
        let _guard = ScopedMux::install(Arc::clone(&mux));
        let (launcher, write_entered, release_write) =
            RecordingPane::new_with_blocking_writer(120, default_domain.domain_id());
        let launcher_dyn: Arc<dyn Pane> = launcher;
        mux.add_pane(&launcher_dyn).expect("add launcher pane");
        let tmux_domain = Arc::new(new_tmux_domain(120));
        let domain: Arc<dyn Domain> = tmux_domain.clone();
        mux.add_domain(&domain)
            .expect("register write-timeout tmux domain");
        *tmux_domain.inner.test_io_deadlines.lock() = Some(TmuxIoDeadlines {
            start: Duration::from_secs(1),
            write: Duration::from_millis(25),
            response: Duration::from_secs(1),
        });
        *tmux_domain.inner.state.lock() = State::Idle;
        tmux_domain
            .inner
            .cmd_queue
            .lock()
            .push_back(Box::new(ListCommands))
            .expect("write-timeout command admission");

        tmux_domain.inner.send_next_command();
        write_entered
            .recv_timeout(Duration::from_secs(5))
            .expect("bounded writer should start");
        wait_until("tmux write deadline terminal outcome", || {
            *tmux_domain.inner.state.lock() == State::Exit
        });
        assert!(tmux_domain.inner.cmd_queue.lock().is_closed());

        release_write
            .send(())
            .expect("release timed-out test writer");
    }

    #[test]
    fn tmux_silent_peer_response_timeout_fences_late_guarded_response() {
        let default_domain: Arc<dyn Domain> =
            Arc::new(LocalDomain::new("tmux-response-timeout-default").expect("local domain"));
        let mux = Arc::new(Mux::new(Some(Arc::clone(&default_domain))));
        let _guard = ScopedMux::install(Arc::clone(&mux));
        let launcher = RecordingPane::new(121, default_domain.domain_id());
        let launcher_dyn: Arc<dyn Pane> = launcher.clone();
        mux.add_pane(&launcher_dyn).expect("add launcher pane");
        let tmux_domain = Arc::new(new_tmux_domain(121));
        let domain: Arc<dyn Domain> = tmux_domain.clone();
        mux.add_domain(&domain)
            .expect("register response-timeout tmux domain");
        *tmux_domain.inner.test_io_deadlines.lock() = Some(TmuxIoDeadlines {
            start: Duration::from_secs(1),
            write: Duration::from_secs(1),
            response: Duration::from_millis(25),
        });
        *tmux_domain.inner.state.lock() = State::Idle;
        tmux_domain
            .inner
            .cmd_queue
            .lock()
            .push_back(Box::new(ListCommands))
            .expect("response-timeout command admission");

        tmux_domain.inner.send_next_command();
        wait_until("tmux response deadline terminal outcome", || {
            *tmux_domain.inner.state.lock() == State::Exit
        });
        assert_eq!(launcher.recorded_writes(), b"list-commands\n".to_vec());

        tmux_domain
            .inner
            .advance(Box::new(vec![Event::Guarded(successful_guarded_response())]));
        assert_eq!(
            *tmux_domain.inner.state.lock(),
            State::Exit,
            "a late response must not resurrect or complete a newer generation"
        );
        assert!(tmux_domain.inner.cmd_queue.lock().is_closed());
    }

    #[test]
    fn tmux_explicit_detach_has_a_clean_exit_deadline() {
        let default_domain: Arc<dyn Domain> =
            Arc::new(LocalDomain::new("tmux-detach-timeout-default").expect("local domain"));
        let mux = Arc::new(Mux::new(Some(Arc::clone(&default_domain))));
        let _guard = ScopedMux::install(Arc::clone(&mux));
        let launcher = RecordingPane::new(122, default_domain.domain_id());
        let launcher_dyn: Arc<dyn Pane> = launcher.clone();
        mux.add_pane(&launcher_dyn).expect("add launcher pane");
        let tmux_domain = Arc::new(new_tmux_domain(122));
        let domain: Arc<dyn Domain> = tmux_domain.clone();
        mux.add_domain(&domain)
            .expect("register detach-timeout tmux domain");
        *tmux_domain.inner.test_io_deadlines.lock() = Some(TmuxIoDeadlines {
            start: Duration::from_secs(1),
            write: Duration::from_secs(1),
            response: Duration::from_millis(25),
        });
        *tmux_domain.inner.state.lock() = State::Idle;

        tmux_domain.detach().expect("admit explicit detach");
        tmux_domain.inner.send_next_command();
        wait_until("serialized tmux detach write", || {
            launcher.recorded_writes() == b"detach\n"
        });
        let completed = tmux_domain
            .inner
            .process_protocol_events(vec![Event::Guarded(successful_guarded_response())])
            .expect("detach Guarded must release only its response phase");
        tmux_domain
            .inner
            .complete_command_response(completed.0, &completed.1);
        wait_until("tmux detach clean-exit deadline", || {
            *tmux_domain.inner.state.lock() == State::Exit
        });
        assert_eq!(launcher.recorded_writes(), b"detach\n".to_vec());
        assert!(launcher.recorded_keys().is_empty());
    }

    #[test]
    fn tmux_split_promise_has_the_command_generation_response_deadline() {
        let default_domain: Arc<dyn Domain> =
            Arc::new(LocalDomain::new("tmux-split-timeout-default").expect("local domain"));
        let mux = Arc::new(Mux::new(Some(Arc::clone(&default_domain))));
        let _guard = ScopedMux::install(Arc::clone(&mux));
        let launcher = RecordingPane::new(123, default_domain.domain_id());
        let launcher_dyn: Arc<dyn Pane> = launcher;
        mux.add_pane(&launcher_dyn).expect("add launcher pane");
        let tmux_domain = Arc::new(new_tmux_domain(123));
        let domain: Arc<dyn Domain> = tmux_domain.clone();
        mux.add_domain(&domain)
            .expect("register split-timeout tmux domain");
        *tmux_domain.inner.test_io_deadlines.lock() = Some(TmuxIoDeadlines {
            start: Duration::from_secs(1),
            write: Duration::from_secs(1),
            response: Duration::from_millis(25),
        });
        *tmux_domain.inner.state.lock() = State::Idle;
        let mut split_promise = promise::Promise::new();
        let split_future = split_promise
            .get_future()
            .expect("create split-timeout future");
        assert!(tmux_domain
            .inner
            .pending_splits
            .lock()
            .insert(44, split_promise)
            .is_none());
        tmux_domain
            .inner
            .cmd_queue
            .lock()
            .push_back(Box::new(SplitPane {
                pane_id: 7,
                direction: SplitDirection::Horizontal,
                request_id: 44,
            }))
            .expect("split-timeout command admission");

        tmux_domain.inner.send_next_command();
        let err = block_on(split_future).expect_err("silent split must reach a terminal outcome");

        assert!(
            err.to_string().contains("split request 44"),
            "unexpected split timeout error: {:#}",
            err
        );
        assert_eq!(*tmux_domain.inner.state.lock(), State::Exit);
        assert!(tmux_domain.inner.pending_splits.lock().is_empty());
    }

    #[test]
    fn tmux_terminal_queue_rejects_stale_producers_and_session_events() {
        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(mux);
        let tmux_domain = new_tmux_domain(93);

        tmux_domain.inner.transition_to_exit_and_schedule_detach();
        assert_eq!(
            tmux_domain
                .inner
                .cmd_queue
                .lock()
                .push_back(Box::new(ListCommands)),
            Err(TmuxEnqueueError::Closed),
            "closed tmux mailbox must reject stale commands"
        );
        tmux_domain
            .inner
            .advance(Box::new(vec![Event::SessionChanged {
                session: 17,
                name: "stale".to_string(),
            }]));

        assert_eq!(*tmux_domain.inner.state.lock(), State::Exit);
        assert_eq!(*tmux_domain.inner.tmux_session.lock(), None);
        assert!(tmux_domain.inner.notification_sub_id.lock().is_none());
        let queue = tmux_domain.inner.cmd_queue.lock();
        assert!(queue.is_closed());
        assert!(queue.is_empty());
    }

    #[test]
    fn tmux_terminal_lifecycle_discards_stale_command_results() {
        let tmux_domain = new_tmux_domain(94);
        let processed = Arc::new(AtomicUsize::new(0));
        tmux_domain.inner.transition_to_exit_and_schedule_detach();

        tmux_domain.inner.process_command_result(
            Box::new(CountingCommand {
                processed: Arc::clone(&processed),
            }),
            &Guarded {
                error: false,
                timestamp: 0,
                number: 0,
                flags: 0,
                output: String::new(),
            },
        );

        assert_eq!(processed.load(Ordering::Relaxed), 0);
    }

    fn install_in_flight_session_order_command(
        inner: &Arc<TmuxDomainState>,
        processed: Arc<AtomicBool>,
    ) {
        let mut queue = inner.cmd_queue.lock();
        queue.in_flight = Some(InFlightTmuxCommand {
            command: Box::new(AssertSessionUnsetCommand {
                owner: Arc::downgrade(inner),
                processed,
            }),
            generation: 1,
            remaining_responses: 1,
            first_error: None,
        });
        queue.retained_by_class[TmuxCommandClass::RequiredControl.index()] = 1;
        drop(queue);
        *inner.state.lock() = State::WaitingForResponse;
        assert!(inner.install_io_operation(TmuxIoOperationKind::Command, 1));
    }

    fn successful_guarded_response() -> Guarded {
        Guarded {
            error: false,
            timestamp: 0,
            number: 0,
            flags: 0,
            output: String::new(),
        }
    }

    #[test]
    fn guarded_callback_precedes_same_batch_protocol_tail() {
        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(Arc::clone(&mux));
        let tmux_domain = Arc::new(new_tmux_domain(97));
        let domain: Arc<dyn Domain> = tmux_domain.clone();
        mux.add_domain(&domain).expect("register tmux test domain");
        let processed = Arc::new(AtomicBool::new(false));
        install_in_flight_session_order_command(&tmux_domain.inner, Arc::clone(&processed));

        let completed = tmux_domain
            .inner
            .process_protocol_events(vec![
                Event::Guarded(successful_guarded_response()),
                Event::SessionChanged {
                    session: 17,
                    name: "ordered".to_string(),
                },
            ])
            .expect("final response must activate the protocol barrier");

        assert_eq!(*tmux_domain.inner.tmux_session.lock(), None);
        assert_eq!(
            tmux_domain.inner.protocol_barrier.lock().events.len(),
            1,
            "same-batch tail must wait behind the response callback"
        );
        tmux_domain
            .inner
            .complete_command_response(completed.0, &completed.1);

        assert!(processed.load(Ordering::Acquire));
        assert_eq!(*tmux_domain.inner.tmux_session.lock(), Some(17));
        assert!(!tmux_domain.inner.protocol_barrier.lock().active);
        assert_eq!(*tmux_domain.inner.state.lock(), State::Idle);
    }

    #[test]
    fn guarded_callback_precedes_cross_batch_protocol_tail() {
        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(Arc::clone(&mux));
        let tmux_domain = Arc::new(new_tmux_domain(98));
        let domain: Arc<dyn Domain> = tmux_domain.clone();
        mux.add_domain(&domain).expect("register tmux test domain");
        let processed = Arc::new(AtomicBool::new(false));
        install_in_flight_session_order_command(&tmux_domain.inner, Arc::clone(&processed));

        let completed = tmux_domain
            .inner
            .process_protocol_events(vec![Event::Guarded(successful_guarded_response())])
            .expect("final response must activate the protocol barrier");
        tmux_domain
            .inner
            .advance(Box::new(vec![Event::SessionChanged {
                session: 23,
                name: "cross-batch".to_string(),
            }]));

        assert_eq!(*tmux_domain.inner.tmux_session.lock(), None);
        assert_eq!(tmux_domain.inner.protocol_barrier.lock().events.len(), 1);
        tmux_domain
            .inner
            .complete_command_response(completed.0, &completed.1);

        assert!(processed.load(Ordering::Acquire));
        assert_eq!(*tmux_domain.inner.tmux_session.lock(), Some(23));
        assert!(!tmux_domain.inner.protocol_barrier.lock().active);
        assert_eq!(*tmux_domain.inner.state.lock(), State::Idle);
    }

    #[test]
    fn guarded_response_queued_behind_final_response_fails_closed() {
        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(Arc::clone(&mux));
        let tmux_domain = Arc::new(new_tmux_domain(99));
        let domain: Arc<dyn Domain> = tmux_domain.clone();
        mux.add_domain(&domain).expect("register tmux test domain");
        let processed = Arc::new(AtomicUsize::new(0));
        {
            let mut queue = tmux_domain.inner.cmd_queue.lock();
            queue.in_flight = Some(InFlightTmuxCommand {
                command: Box::new(CountingCommand {
                    processed: Arc::clone(&processed),
                }),
                generation: 1,
                remaining_responses: 1,
                first_error: None,
            });
            queue.retained_by_class[TmuxCommandClass::RequiredControl.index()] = 1;
        }
        *tmux_domain.inner.state.lock() = State::WaitingForResponse;
        assert!(tmux_domain
            .inner
            .install_io_operation(TmuxIoOperationKind::Command, 1));

        let completed = tmux_domain
            .inner
            .process_protocol_events(vec![
                Event::Guarded(successful_guarded_response()),
                Event::Guarded(successful_guarded_response()),
            ])
            .expect("the owned response must activate the protocol barrier");
        tmux_domain
            .inner
            .complete_command_response(completed.0, &completed.1);

        assert_eq!(processed.load(Ordering::Relaxed), 1);
        assert_eq!(*tmux_domain.inner.state.lock(), State::Exit);
        assert!(
            tmux_domain.inner.protocol_barrier.lock().events.is_empty(),
            "terminalization must discard the desynchronized protocol tail"
        );
    }

    #[test]
    fn protocol_barrier_rejects_retained_payload_overflow_atomically() {
        let mut barrier = TmuxProtocolBarrier::default();
        let oversized = Event::Output {
            pane: 1,
            text: vec![0_u8; PROTOCOL_BARRIER_MAX_BYTES.saturating_add(1)],
        };

        assert!(barrier.activate(0, vec![oversized]).is_err());
        assert!(barrier.events.is_empty());
        assert_eq!(barrier.retained_bytes, 0);
    }

    #[test]
    fn protocol_barrier_counts_paste_buffer_names_against_byte_cap() {
        let changed_buffer = String::with_capacity(64);
        let deleted_buffer = String::with_capacity(64);
        let response_bytes = PROTOCOL_BARRIER_MAX_BYTES
            .checked_sub(std::mem::size_of::<Event>())
            .expect("protocol barrier byte cap must exceed one inline event");

        for event in [
            Event::PasteBufferChanged {
                buffer: changed_buffer,
            },
            Event::PasteBufferDeleted {
                buffer: deleted_buffer,
            },
        ] {
            let mut barrier = TmuxProtocolBarrier::default();
            assert!(
                barrier.activate(response_bytes, vec![event]).is_err(),
                "paste-buffer heap storage must count against the barrier byte cap"
            );
        }
    }

    #[test]
    fn protocol_barrier_clear_releases_deque_storage_after_drain_and_abort() {
        for drain_before_clear in [true, false] {
            let mut barrier = TmuxProtocolBarrier::default();
            barrier
                .activate(0, vec![Event::SessionsChanged; 1_024])
                .expect("test protocol events must fit within the barrier");
            let high_water_capacity = barrier.events.capacity();
            assert!(high_water_capacity >= 1_024);

            if drain_before_clear {
                while barrier.pop_front().is_some() {}
            }

            let retired_storage = barrier.clear();
            assert_eq!(
                barrier.events.capacity(),
                0,
                "the live barrier must not retain its deque high-water allocation"
            );
            assert!(!barrier.active);
            assert_eq!(barrier.retained_bytes, 0);
            assert_eq!(barrier.response_retained_bytes, 0);
            assert_eq!(retired_storage.capacity(), high_water_capacity);
            assert_eq!(
                retired_storage.is_empty(),
                drain_before_clear,
                "successful drain returns empty storage; terminal abort returns queued events"
            );
            drop(retired_storage);
        }
    }

    #[test]
    fn tmux_clean_exit_closes_mailbox_releases_resources_and_removes_domain() {
        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(Arc::clone(&mux));
        let tmux_domain = Arc::new(new_tmux_domain(95));
        let domain: Arc<dyn Domain> = tmux_domain.clone();
        mux.add_domain(&domain)
            .expect("test tmux domain should register");

        assert!(tmux_domain
            .inner
            .cmd_queue
            .lock()
            .push_back(Box::new(ListCommands))
            .is_ok());
        tmux_domain
            .inner
            .support_commands
            .lock()
            .insert("list-panes".to_string(), "list-panes".to_string());
        *tmux_domain.inner.attach_state.lock() = AttachState::Done;
        let provisional_window = mux.new_empty_window(Some("tmux-test".to_string()), None);
        *tmux_domain.inner.gui_window.lock() = Some(provisional_window);

        tmux_domain.inner.transition_to_clean_exit();

        assert_eq!(*tmux_domain.inner.state.lock(), State::Exit);
        assert!(
            tmux_domain
                .inner
                .clean_detach_completed
                .load(Ordering::Acquire),
            "clean launcher exit must synchronously remove the drained domain"
        );
        assert!(mux.get_domain(tmux_domain.domain_id()).is_none());
        assert!(tmux_domain.inner.gui_window.lock().is_none());
        assert!(tmux_domain.inner.support_commands.lock().is_empty());
        assert_eq!(*tmux_domain.inner.attach_state.lock(), AttachState::Init);
        let lifecycle = tmux_domain.inner.lifecycle.lock();
        assert!(lifecycle.terminal);
        assert!(lifecycle.clean_exit);
        assert!(lifecycle.resources_cleaned);
        assert_eq!(lifecycle.active_operations, 0);
        drop(lifecycle);
        let queue = tmux_domain.inner.cmd_queue.lock();
        assert!(queue.is_closed());
        assert!(queue.is_empty());
    }

    #[test]
    fn tmux_clean_exit_waits_for_admitted_operation_before_domain_removal() {
        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(Arc::clone(&mux));
        let tmux_domain = Arc::new(new_tmux_domain(96));
        let domain: Arc<dyn Domain> = tmux_domain.clone();
        mux.add_domain(&domain)
            .expect("test tmux domain should register");

        let active_operation = tmux_domain
            .inner
            .begin_active_operation()
            .expect("live domain must admit operation");

        tmux_domain.inner.transition_to_clean_exit();

        assert!(
            mux.get_domain(tmux_domain.domain_id()).is_some(),
            "clean removal must wait for an already-admitted operation"
        );
        assert!(tmux_domain
            .inner
            .clean_exit_requested
            .load(Ordering::Acquire));
        drop(active_operation);
        assert!(
            mux.get_domain(tmux_domain.domain_id()).is_none(),
            "dropping the final operation lease must complete clean removal"
        );
    }

    #[test]
    fn tmux_terminal_transition_is_reentrant_from_active_callback() {
        let tmux_domain = new_tmux_domain(97);
        assert!(tmux_domain
            .inner
            .with_active_lifecycle(|| {
                tmux_domain.inner.transition_to_exit_and_schedule_detach();
            })
            .is_some());

        assert_eq!(*tmux_domain.inner.state.lock(), State::Exit);
        let lifecycle = tmux_domain.inner.lifecycle.lock();
        assert!(lifecycle.terminal);
        assert_eq!(lifecycle.active_operations, 0);
        assert!(lifecycle.resources_cleaned);
    }

    #[test]
    fn tmux_send_schedule_is_single_flight_and_rearms_after_lease_drop() {
        let tmux_domain = new_tmux_domain(98);
        assert!(
            tmux_domain.inner.try_claim_send_schedule(),
            "first runnable must claim the scheduling edge"
        );
        let first = SendScheduleLease {
            owner: Arc::clone(&tmux_domain.inner),
            completed: true,
        };
        assert!(
            !tmux_domain.inner.try_claim_send_schedule(),
            "a producer burst must not allocate a second sender runnable"
        );
        drop(first);
        assert!(
            tmux_domain.inner.try_claim_send_schedule(),
            "dropping or cancelling the runnable must rearm scheduling"
        );
        let rearmed = SendScheduleLease {
            owner: Arc::clone(&tmux_domain.inner),
            completed: true,
        };
        drop(rearmed);
    }

    #[test]
    fn lost_tmux_sender_runnable_fails_durable_mailbox_closed() {
        let tmux_domain = new_tmux_domain(99);
        tmux_domain
            .inner
            .cmd_queue
            .lock()
            .push_back(Box::new(ListCommands))
            .expect("required control admission");
        assert!(tmux_domain.inner.try_claim_send_schedule());
        let lost = SendScheduleLease {
            owner: Arc::clone(&tmux_domain.inner),
            completed: false,
        };

        drop(lost);

        assert_eq!(*tmux_domain.inner.state.lock(), State::Exit);
        let queue = tmux_domain.inner.cmd_queue.lock();
        assert!(queue.is_closed());
        assert!(queue.is_empty());
    }

    #[test]
    fn notification_intent_storm_schedules_once_per_domain_and_stays_two_slot_bounded() {
        const DOMAINS: usize = 32;
        const EVENTS_PER_DOMAIN: u64 = 4_096;
        let mut scheduled = 0usize;
        let mut coalesced = 0u64;

        for domain_offset in 0..DOMAINS {
            let mut state = TmuxNotificationIntentState::default();
            for revision in 1..=EVENTS_PER_DOMAIN {
                let intent = if revision % 2 == 0 {
                    TmuxNotificationIntent::WindowInvalidated(domain_offset)
                } else {
                    TmuxNotificationIntent::PaneFocused(domain_offset)
                };
                let offer = state.offer(SequencedTmuxNotificationIntent {
                    revision: TopologyRevision::new(revision),
                    intent,
                });
                scheduled += usize::from(offer.schedule);
                coalesced = coalesced.saturating_add(u64::from(offer.coalesced));
                assert!(
                    state.pending_len() <= 2,
                    "a notification storm must never retain more than one intent per kind"
                );
            }

            let batch = state.take_ordered_batch();
            assert_eq!(
                batch[0].map(|intent| intent.revision),
                Some(TopologyRevision::new(EVENTS_PER_DOMAIN - 1))
            );
            assert_eq!(
                batch[1].map(|intent| intent.revision),
                Some(TopologyRevision::new(EVENTS_PER_DOMAIN))
            );
            assert_eq!(
                state.finish_quantum(),
                TmuxNotificationIntentRunDisposition::Idle
            );
        }

        assert_eq!(
            scheduled, DOMAINS,
            "runnable growth must follow active domains, not notification count"
        );
        assert_eq!(
            coalesced,
            (DOMAINS as u64).saturating_mul(EVENTS_PER_DOMAIN - 2),
            "all but the first pending intent of each kind should coalesce"
        );
    }

    #[test]
    fn notification_intents_preserve_cross_kind_revision_order_and_reject_delayed_regressions() {
        let mut state = TmuxNotificationIntentState::default();
        assert!(
            state
                .offer(SequencedTmuxNotificationIntent {
                    revision: TopologyRevision::new(8),
                    intent: TmuxNotificationIntent::WindowInvalidated(80),
                })
                .schedule
        );
        assert!(
            !state
                .offer(SequencedTmuxNotificationIntent {
                    revision: TopologyRevision::new(7),
                    intent: TmuxNotificationIntent::PaneFocused(70),
                })
                .schedule
        );

        let batch = state.take_ordered_batch();
        assert_eq!(
            batch,
            [
                Some(SequencedTmuxNotificationIntent {
                    revision: TopologyRevision::new(7),
                    intent: TmuxNotificationIntent::PaneFocused(70),
                }),
                Some(SequencedTmuxNotificationIntent {
                    revision: TopologyRevision::new(8),
                    intent: TmuxNotificationIntent::WindowInvalidated(80),
                }),
            ]
        );

        let delayed = state.offer(SequencedTmuxNotificationIntent {
            revision: TopologyRevision::new(6),
            intent: TmuxNotificationIntent::PaneFocused(60),
        });
        assert!(delayed.coalesced);
        assert!(!delayed.schedule);
        assert_eq!(state.pending_len(), 0);
        assert_eq!(
            state.finish_quantum(),
            TmuxNotificationIntentRunDisposition::Idle
        );
    }

    #[test]
    fn notification_topology_barrier_holds_newer_cross_kind_intent_until_gap_arrives() {
        let mut state = TmuxNotificationIntentState::default();
        state
            .initialize_topology_order(TopologyRevision::INITIAL)
            .expect("initialize topology ordering");

        let newer = state
            .observe_topology_event(
                TopologyRevision::new(2),
                TmuxTopologyBarrierEvent::Intent(
                    TmuxNotificationIntent::WindowInvalidated(20),
                ),
            )
            .expect("buffer newer revision");
        assert!(!newer.schedule);
        assert_eq!(state.pending_len(), 0);

        let contiguous = state
            .observe_topology_event(
                TopologyRevision::new(1),
                TmuxTopologyBarrierEvent::Intent(TmuxNotificationIntent::PaneFocused(10)),
            )
            .expect("close revision gap");
        assert!(contiguous.schedule);
        let batch = state.take_ordered_batch();
        assert_eq!(
            batch,
            [
                Some(SequencedTmuxNotificationIntent {
                    revision: TopologyRevision::new(1),
                    intent: TmuxNotificationIntent::PaneFocused(10),
                }),
                Some(SequencedTmuxNotificationIntent {
                    revision: TopologyRevision::new(2),
                    intent: TmuxNotificationIntent::WindowInvalidated(20),
                }),
            ],
            "a newer window intent must never overtake an older pane intent"
        );
    }

    #[test]
    fn notification_topology_barriers_advance_irrelevant_revisions_and_reject_duplicates() {
        let mut state = TmuxNotificationIntentState::default();
        state
            .initialize_topology_order(TopologyRevision::new(4))
            .expect("initialize topology ordering");

        let buffered = state
            .observe_topology_event(
                TopologyRevision::new(6),
                TmuxTopologyBarrierEvent::Intent(TmuxNotificationIntent::PaneFocused(60)),
            )
            .expect("buffer revision six");
        assert!(!buffered.schedule);
        assert!(
            state
                .observe_topology_event(
                    TopologyRevision::new(6),
                    TmuxTopologyBarrierEvent::Barrier,
                )
                .is_err(),
            "duplicate authority must fail closed instead of replacing retained work"
        );

        let advanced = state
            .observe_topology_event(
                TopologyRevision::new(5),
                TmuxTopologyBarrierEvent::Barrier,
            )
            .expect("irrelevant revision closes the gap");
        assert!(advanced.schedule);
        assert_eq!(
            state.take_ordered_batch()[0],
            Some(SequencedTmuxNotificationIntent {
                revision: TopologyRevision::new(6),
                intent: TmuxNotificationIntent::PaneFocused(60),
            })
        );
    }

    #[test]
    fn notification_topology_reorder_gap_is_bounded_and_fails_closed() {
        let mut state = TmuxNotificationIntentState::default();
        state
            .initialize_topology_order(TopologyRevision::INITIAL)
            .expect("initialize topology ordering");
        let first_rejected_revision = u64::try_from(NOTIFICATION_INTENT_MAX_REORDER_GAP)
            .expect("reorder cap fits u64")
            .saturating_add(1);

        let err = state
            .observe_topology_event(
                TopologyRevision::new(first_rejected_revision),
                TmuxTopologyBarrierEvent::Barrier,
            )
            .expect_err("an unbounded topology gap must fail closed");

        assert!(
            err.to_string().contains("exceeds bounded window"),
            "gap rejection should retain a diagnosable reason: {:#}",
            err
        );
        assert!(state.topology_reorder.is_empty());
        assert_eq!(state.pending_len(), 0);
    }

    #[test]
    fn notification_intent_capacity_wait_rearms_without_losing_newer_final_state() {
        let mut state = TmuxNotificationIntentState::default();
        assert!(
            state
                .offer(SequencedTmuxNotificationIntent {
                    revision: TopologyRevision::new(1),
                    intent: TmuxNotificationIntent::PaneFocused(10),
                })
                .schedule
        );
        let batch = state.take_ordered_batch();
        let first = batch[0].expect("one pending pane intent");
        assert_eq!(state.wait_for_capacity(first, None), 0);
        assert!(state.is_waiting_for_capacity());

        let replacement = state.offer(SequencedTmuxNotificationIntent {
            revision: TopologyRevision::new(2),
            intent: TmuxNotificationIntent::PaneFocused(20),
        });
        assert!(replacement.coalesced);
        assert!(!replacement.schedule);
        assert_eq!(state.pending_len(), 1);

        assert!(
            state.capacity_available(),
            "the first capacity edge must claim exactly one retry runnable"
        );
        assert!(!state.capacity_available());
        let retried = state.take_ordered_batch();
        assert_eq!(
            retried[0],
            Some(SequencedTmuxNotificationIntent {
                revision: TopologyRevision::new(2),
                intent: TmuxNotificationIntent::PaneFocused(20),
            })
        );
        assert_eq!(
            state.finish_quantum(),
            TmuxNotificationIntentRunDisposition::Idle
        );
    }

    #[test]
    fn notification_intent_close_absorbs_capacity_requeue_and_reinitialization() {
        let mut state = TmuxNotificationIntentState::default();
        assert!(
            state
                .offer(SequencedTmuxNotificationIntent {
                    revision: TopologyRevision::new(1),
                    intent: TmuxNotificationIntent::PaneFocused(10),
                })
                .schedule
        );
        let failed = state.take_ordered_batch()[0].expect("one pending pane intent");

        state.close();

        assert_eq!(
            state.wait_for_capacity(failed, None),
            1,
            "terminal closure must count and discard the unadmitted intent"
        );
        assert_eq!(state.pending_len(), 0);
        assert!(!state.is_waiting_for_capacity());
        assert_eq!(
            state.finish_quantum(),
            TmuxNotificationIntentRunDisposition::Closed
        );
        assert!(
            state
                .initialize_topology_order(TopologyRevision::INITIAL)
                .is_err(),
            "closed topology coordination must remain absorbing"
        );
    }

    #[test]
    fn tmux_mirror_index_is_bidirectional_and_rejects_identity_aliases() {
        let mut index = TmuxMirrorIndex::default();
        index.register_pane(11, 101).expect("register pane");
        index.register_window(22, 202).expect("register window");
        assert_eq!(index.remote_pane_for_local(11), Some(101));
        assert_eq!(index.remote_window_for_local_tab(22), Some(202));

        assert!(index.register_pane(11, 102).is_err());
        assert!(index.register_pane(12, 101).is_err());
        assert!(index.register_window(22, 203).is_err());
        assert!(index.register_window(23, 202).is_err());

        assert_eq!(index.unregister_pane(101).expect("unregister pane"), Some(11));
        assert_eq!(
            index.unregister_window(202).expect("unregister window"),
            Some(22)
        );
        assert_eq!(index.remote_pane_for_local(11), None);
        assert_eq!(index.remote_window_for_local_tab(22), None);
    }

    #[test]
    fn notification_intent_telemetry_exposes_required_counters() {
        let telemetry = TmuxNotificationIntentTelemetry::default();
        telemetry.record_received();
        telemetry.record_prefiltered();
        telemetry.record_coalesced(3);
        telemetry.record_scheduled();
        telemetry.record_applied();
        telemetry.record_dropped_stale();
        telemetry.record_backpressured();

        assert_eq!(
            telemetry.snapshot(),
            TmuxNotificationIntentTelemetrySnapshot {
                received: 1,
                prefiltered: 1,
                coalesced: 3,
                scheduled: 1,
                applied: 1,
                dropped_stale: 1,
                backpressured: 1,
            }
        );
    }

    #[test]
    fn tmux_domain_state_reports_detached_after_exit() {
        let tmux_domain = new_tmux_domain(0);
        *tmux_domain.inner.state.lock() = State::Exit;
        assert_eq!(tmux_domain.state(), DomainState::Detached);
    }

    #[test]
    fn tmux_backlog_oversized_payload_requires_resync_instead_of_suffix_replay() {
        let mut backlog = TmuxBacklog::default();
        let limits = TmuxBacklogLimits::new(8, 32, 4);
        backlog.append_with_limits(1, b"\x1b]8;;https://example.com\x1b\\text", limits);

        assert!(backlog.pane_requires_resync(1));
        assert_eq!(backlog.pane_bytes(1), Some(Vec::new()));
        assert_eq!(backlog.total_bytes(), 0);
        assert_eq!(
            backlog.retained_byte_capacity(),
            0,
            "gapping a pane must release its byte allocation, not only logical length",
        );
        assert_eq!(
            backlog.take(1),
            Some(TmuxBacklogDrain::ResyncRequired),
            "a cut inside OSC/UTF-8 must never replay an arbitrary suffix",
        );
    }

    #[test]
    fn tmux_backlog_enforces_aggregate_and_entry_lru_limits() {
        let mut backlog = TmuxBacklog::default();
        let limits = TmuxBacklogLimits::new(8, 10, 2);
        backlog.append_with_limits(1, b"abcdef", limits);
        backlog.append_with_limits(2, b"ghij", limits);
        backlog.append_with_limits(1, b"k", limits);

        assert_eq!(backlog.pane_bytes(1), Some(b"abcdefk".to_vec()));
        assert!(backlog.pane_requires_resync(2));
        assert_eq!(backlog.total_bytes(), 7);

        backlog.append_with_limits(3, b"zz", limits);
        assert!(
            backlog.requires_global_resync(),
            "evicting a bounded per-pane gap marker must promote to global resync",
        );
        assert_eq!(backlog.len(), 0);
        assert_eq!(backlog.total_bytes(), 0);
        assert_eq!(
            backlog.retained_byte_capacity(),
            0,
            "global promotion must release all per-pane allocations",
        );
    }

    #[test]
    fn tmux_backlog_zero_limits_disable_retention() {
        for limits in [
            TmuxBacklogLimits::new(0, 32, 4),
            TmuxBacklogLimits::new(8, 0, 4),
            TmuxBacklogLimits::new(8, 32, 0),
        ] {
            let mut backlog = TmuxBacklog::default();
            backlog.append_with_limits(1, b"discarded", limits);
            assert_eq!(backlog.len(), 0);
            assert_eq!(backlog.total_bytes(), 0);
            assert!(
                backlog.take_global_resync(),
                "disabled retention must preserve a bounded resync obligation",
            );
        }
    }

    #[test]
    fn tmux_backlog_take_remove_and_hot_limit_shrink_preserve_accounting() {
        let mut backlog = TmuxBacklog::default();
        let roomy = TmuxBacklogLimits::new(16, 32, 4);
        backlog.append_with_limits(1, b"0123456789", roomy);
        backlog.append_with_limits(2, b"abcdef", roomy);
        assert_eq!(backlog.total_bytes(), 16);

        let tight = TmuxBacklogLimits::new(4, 6, 4);
        backlog.append_with_limits(2, b"", tight);
        assert!(backlog.pane_requires_resync(1));
        assert!(backlog.pane_requires_resync(2));
        assert_eq!(backlog.total_bytes(), 0);
        assert_eq!(
            backlog.retained_byte_capacity(),
            0,
            "hot contraction must release every gapped allocation",
        );

        assert_eq!(backlog.take(2), Some(TmuxBacklogDrain::ResyncRequired));
        assert_eq!(backlog.total_bytes(), 0);
        assert!(backlog.remove(1));
        assert_eq!(backlog.total_bytes(), 0);
        backlog.clear();
        assert_eq!(backlog.len(), 0);
    }

    #[test]
    fn cmd_queue_hard_cap_rejects_new_work_without_discarding_acknowledged_commands() {
        let default_domain: Arc<dyn Domain> =
            Arc::new(LocalDomain::new("tmux-queue-cap-default").expect("local domain"));
        let mux = Arc::new(Mux::new(Some(Arc::clone(&default_domain))));
        let _guard = ScopedMux::install(Arc::clone(&mux));

        let launcher = RecordingPane::new(99, default_domain.domain_id());
        let launcher_dyn: Arc<dyn Pane> = launcher.clone();
        mux.add_pane(&launcher_dyn).expect("add launcher pane");

        let tmux_domain = new_tmux_domain(99);
        // Required control retains terminal progress authority even if the
        // sender is stalled and never reaches a pop site.
        {
            let mut queue = tmux_domain.inner.cmd_queue.lock();
            for _ in 0..CMD_QUEUE_MAX_DEPTH - CMD_QUEUE_TERMINAL_RESERVED_SLOTS {
                assert!(queue.push_back(Box::new(ListCommands)).is_ok());
            }
            for _ in 0..100 {
                assert_eq!(
                    queue.push_back(Box::new(ListCommands)),
                    Err(TmuxEnqueueError::Full)
                );
            }
            for sequence in 0..CMD_QUEUE_TERMINAL_RESERVED_SLOTS {
                assert!(queue
                    .push_back(Box::new(ClassedTestCommand {
                        class: TmuxCommandClass::TerminalControl,
                        sequence,
                    }))
                    .is_ok());
            }
            assert_eq!(
                queue.push_back(Box::new(ClassedTestCommand {
                    class: TmuxCommandClass::TerminalControl,
                    sequence: CMD_QUEUE_TERMINAL_RESERVED_SLOTS,
                })),
                Err(TmuxEnqueueError::Full)
            );
            assert_eq!(queue.len(), CMD_QUEUE_MAX_DEPTH);
            assert_eq!(queue.rejected_commands, 101);
        }
    }

    #[test]
    fn cmd_queue_detach_barrier_preempts_queued_work_and_rejects_later_producers() {
        let mut queue = TmuxCmdQueue::new();
        queue
            .push_back(Box::new(ClassedTestCommand {
                class: TmuxCommandClass::RequiredControl,
                sequence: 1,
            }))
            .expect("admit work preceding detach request");
        queue
            .push_back(Box::new(ClassedTestCommand {
                class: TmuxCommandClass::CoalescibleIntent,
                sequence: 2,
            }))
            .expect("admit coalescible work preceding detach request");

        queue
            .push_domain_detach(Box::new(DetachClient))
            .expect("terminal detach barrier admission");
        assert!(queue.has_domain_detach_pending());
        assert_eq!(
            queue
                .front()
                .expect("detach barrier is serviceable")
                .get_command(0),
            "detach\n"
        );
        assert_eq!(
            queue.push_back(Box::new(ListCommands)),
            Err(TmuxEnqueueError::Closed),
            "new work must not cross the terminal barrier"
        );
        assert_eq!(
            queue.push_required_batch(vec![Box::new(ListCommands)]),
            Err(TmuxEnqueueError::Closed),
            "required batches must not cross the terminal barrier"
        );
        assert_eq!(
            queue.push_domain_detach(Box::new(DetachClient)),
            Err(TmuxEnqueueError::Closed),
            "the explicit detach barrier is single-flight"
        );

        let completed = complete_next_mailbox_command(&mut queue);
        assert!(completed.awaits_clean_exit());
        assert_eq!(completed.get_command(0), "detach\n");
        assert!(
            !queue.has_pending(),
            "work admitted before detach must await terminal cleanup, not run after it"
        );
        assert_eq!(
            queue.len(),
            2,
            "pre-barrier work remains accounted until terminal cleanup"
        );
    }

    #[test]
    fn cmd_queue_payload_cap_rejects_byte_overflow_without_displacing_work() {
        let mut queue = TmuxCmdQueue::new();
        assert!(queue
            .push_back(Box::new(SendKeys {
                pane: 7,
                keys: b"kept".to_vec(),
            }))
            .is_ok());
        queue.payload_bytes = CMD_QUEUE_MAX_PAYLOAD_BYTES - 1;

        assert_eq!(
            queue.push_back(Box::new(SendKeys {
                pane: 8,
                keys: b"xx".to_vec(),
            })),
            Err(TmuxEnqueueError::Full)
        );
        assert_eq!(queue.len(), 1);
        assert_eq!(
            queue
                .front()
                .and_then(|command| command.as_send_keys())
                .map(|(_, keys)| keys),
            Some(b"kept".as_slice())
        );
    }

    #[test]
    fn required_batch_admission_is_atomic_at_command_cap() {
        let mut queue = TmuxCmdQueue::new();
        for _ in 0..CMD_QUEUE_MAX_DEPTH - CMD_QUEUE_TERMINAL_RESERVED_SLOTS - 1 {
            queue
                .push_back(Box::new(ListCommands))
                .expect("test setup should leave one mailbox slot");
        }
        let depth_before = queue.len();

        assert_eq!(
            queue.push_required_batch(vec![Box::new(ListCommands), Box::new(ListCommands)]),
            Err(TmuxEnqueueError::Full),
        );
        assert_eq!(
            queue.len(),
            depth_before,
            "a rejected required batch must not admit a prefix",
        );
    }

    #[test]
    fn required_batch_admission_is_terminal_after_close() {
        let mut queue = TmuxCmdQueue::new();
        let abandoned = queue.close();
        drop(abandoned);

        assert_eq!(
            queue.push_required_batch(vec![Box::new(ListCommands), Box::new(ListCommands)]),
            Err(TmuxEnqueueError::Closed),
        );
        assert!(queue.is_empty());
    }

    #[test]
    fn required_admission_fails_closed_when_sender_cannot_be_registered() {
        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(mux);
        let tmux_domain = new_tmux_domain(404);

        let err = tmux_domain
            .inner
            .enqueue_required(Box::new(ListCommands), "missing-domain test")
            .expect_err("unregistered sender must not acknowledge required work");

        assert!(err.to_string().contains("scheduling failed"));
        assert_eq!(*tmux_domain.inner.state.lock(), State::Exit);
        let queue = tmux_domain.inner.cmd_queue.lock();
        assert!(queue.is_closed());
        assert!(queue.is_empty());
    }

    #[test]
    fn semantic_capacity_reserves_control_and_terminal_progress_at_saturation() {
        let mut queue = TmuxCmdQueue::new();
        let non_control_limit = CMD_QUEUE_MAX_DEPTH - CMD_QUEUE_CONTROL_RESERVED_SLOTS;
        for sequence in 0..non_control_limit {
            queue
                .push_back(Box::new(ClassedTestCommand {
                    class: TmuxCommandClass::LosslessInput,
                    sequence,
                }))
                .expect("lossless input should fill only its bounded non-control capacity");
        }
        assert_eq!(
            queue.push_back(Box::new(ClassedTestCommand {
                class: TmuxCommandClass::LosslessInput,
                sequence: non_control_limit,
            })),
            Err(TmuxEnqueueError::Full),
            "bulk input must not consume required-control progress authority",
        );

        let required_count =
            CMD_QUEUE_CONTROL_RESERVED_SLOTS - CMD_QUEUE_TERMINAL_RESERVED_SLOTS;
        let required_batch: Vec<Box<dyn TmuxCommand>> = (0..required_count)
            .map(|sequence| {
                Box::new(ClassedTestCommand {
                    class: TmuxCommandClass::RequiredControl,
                    sequence,
                }) as Box<dyn TmuxCommand>
            })
            .collect();
        queue
            .push_required_batch(required_batch)
            .expect("required initialization must terminate admission under input saturation");

        for sequence in 0..CMD_QUEUE_TERMINAL_RESERVED_SLOTS {
            queue
                .push_back(Box::new(ClassedTestCommand {
                    class: TmuxCommandClass::TerminalControl,
                    sequence,
                }))
                .expect("terminal control must retain its final progress authority");
        }
        assert_eq!(queue.len(), CMD_QUEUE_MAX_DEPTH);
        assert_eq!(
            queue.retained_by_class[TmuxCommandClass::LosslessInput.index()],
            non_control_limit
        );
        assert_eq!(
            queue.retained_by_class[TmuxCommandClass::RequiredControl.index()],
            required_count
        );
        assert_eq!(
            queue.retained_by_class[TmuxCommandClass::TerminalControl.index()],
            CMD_QUEUE_TERMINAL_RESERVED_SLOTS
        );
    }

    #[test]
    fn required_batch_rejects_misclassified_commands_atomically() {
        let mut queue = TmuxCmdQueue::new();
        assert_eq!(
            queue.push_required_batch(vec![Box::new(ClassedTestCommand {
                class: TmuxCommandClass::LosslessInput,
                sequence: 1,
            })]),
            Err(TmuxEnqueueError::ClassMismatch),
        );
        assert!(queue.is_empty());
    }

    #[test]
    fn coalescible_lane_meets_bounded_service_without_reordering_lossless_input() {
        let mut queue = TmuxCmdQueue::new();
        for sequence in 0..=CMD_QUEUE_DURABLE_SERVICE_QUANTUM {
            queue
                .push_back(Box::new(ClassedTestCommand {
                    class: TmuxCommandClass::LosslessInput,
                    sequence,
                }))
                .expect("durable test command");
        }
        queue
            .push_back(Box::new(ClassedTestCommand {
                class: TmuxCommandClass::CoalescibleIntent,
                sequence: usize::MAX,
            }))
            .expect("coalescible intent");

        for expected in 0..CMD_QUEUE_DURABLE_SERVICE_QUANTUM {
            let completed = complete_next_mailbox_command(&mut queue);
            assert_eq!(completed.mailbox_class(), TmuxCommandClass::LosslessInput);
            assert_eq!(completed.get_command(0), format!("test-{expected}\n"));
        }
        let serviced_intent = complete_next_mailbox_command(&mut queue);
        assert_eq!(
            serviced_intent.mailbox_class(),
            TmuxCommandClass::CoalescibleIntent,
            "one latest-intent command must run after the bounded durable quantum",
        );
        let next_lossless = complete_next_mailbox_command(&mut queue);
        assert_eq!(next_lossless.get_command(0), format!(
            "test-{}\n",
            CMD_QUEUE_DURABLE_SERVICE_QUANTUM
        ));
    }

    #[test]
    fn lossless_key_bytes_preserve_cross_pane_admission_order() {
        let mut queue = TmuxCmdQueue::new();
        for (pane, keys) in [(1, b"a".as_slice()), (2, b"b"), (1, b"c")] {
            queue
                .push_back(Box::new(SendKeys {
                    pane,
                    keys: keys.to_vec(),
                }))
                .expect("lossless key command");
        }

        let mut observed = Vec::new();
        while !queue.is_empty() {
            let completed = complete_next_mailbox_command(&mut queue);
            let (pane, keys) = completed.as_send_keys().expect("send-keys command");
            observed.push((pane, keys.to_vec()));
        }
        assert_eq!(
            observed,
            vec![(1, b"a".to_vec()), (2, b"b".to_vec()), (1, b"c".to_vec())]
        );
    }

    #[test]
    fn resize_remains_in_flight_until_both_guarded_responses_arrive() {
        let mut queue = TmuxCmdQueue::new();
        queue
            .push_back(Box::new(Resize {
                pane_id: 7,
                size: portable_pty::PtySize {
                    rows: 40,
                    cols: 120,
                    pixel_width: 0,
                    pixel_height: 0,
                },
            }))
            .expect("resize should be admitted");
        let resize = queue
            .take_next_for_preparation()
            .expect("resize should enter preparation");
        assert!(queue.install_in_flight(resize, 1));
        queue
            .push_back(Box::new(ListCommands))
            .expect("following command should be admitted");

        let first_error = Guarded {
            error: true,
            timestamp: 1,
            number: 1,
            flags: 0,
            output: "resize-window failed".to_string(),
        };
        assert!(
            queue.record_in_flight_response(&first_error).is_none(),
            "the first resize response must not release the mailbox slot",
        );
        assert!(
            queue
                .front()
                .is_some_and(|command| command.as_resize().is_some()),
            "the resize must remain authoritative until its second response",
        );

        let second_success = Guarded {
            error: false,
            timestamp: 2,
            number: 2,
            flags: 0,
            output: String::new(),
        };
        let (completed, retained_response, generation) = queue
            .record_in_flight_response(&second_success)
            .expect("the second response should complete the resize");
        assert_eq!(generation, 1);
        assert!(completed.as_resize().is_some());
        assert!(
            retained_response.error,
            "the first guarded error must survive a later successful response",
        );
        assert!(
            queue
                .front()
                .is_some_and(|command| command.as_resize().is_none()),
            "the following command must not consume either resize response",
        );
    }

    #[test]
    fn cmd_queue_cap_rejects_overflow_without_displacing_in_flight_request() {
        use crate::tab::SplitDirection;

        let default_domain: Arc<dyn Domain> =
            Arc::new(LocalDomain::new("tmux-ft-wajba-default").expect("local domain"));
        let mux = Arc::new(Mux::new(Some(Arc::clone(&default_domain))));
        let _guard = ScopedMux::install(Arc::clone(&mux));

        let launcher = RecordingPane::new(777, default_domain.domain_id());
        let launcher_dyn: Arc<dyn Pane> = launcher.clone();
        mux.add_pane(&launcher_dyn).expect("add launcher pane");

        let tmux_domain = new_tmux_domain(777);
        let inner = Arc::clone(&tmux_domain.inner);
        let domain_id = inner.domain_id;

        let sentinel_head: Box<dyn TmuxCommand> = Box::new(SplitPane {
            pane_id: 4242,
            direction: SplitDirection::Horizontal,
            request_id: 1,
        });
        let sentinel_cmd_text = sentinel_head.get_command(domain_id);
        // Sanity: the sentinel's text is distinguishable from the
        // bulk-fill command type so an accidental drop-and-shift
        // cannot silently pass this test.
        let bulk_sample_text = ListCommands.get_command(domain_id);
        assert_ne!(
            sentinel_cmd_text, bulk_sample_text,
            "test setup broken: sentinel and bulk cmds produce identical text"
        );

        {
            let mut queue = inner.cmd_queue.lock();
            assert!(
                queue.push_back(sentinel_head).is_ok(),
                "test setup must admit the sentinel before sender preparation"
            );
            let prepared = queue
                .take_next_for_preparation()
                .expect("the admitted sentinel must enter sender preparation");
            assert!(queue.install_in_flight(prepared, 1));
            assert_eq!(queue.len(), 1);
        }

        *inner.state.lock() = State::WaitingForResponse;

        for _ in 0..CMD_QUEUE_MAX_DEPTH - CMD_QUEUE_TERMINAL_RESERVED_SLOTS - 1 {
            let mut queue = inner.cmd_queue.lock();
            assert!(inner
                .push_command_capped(&mut queue, Box::new(ListCommands))
                .is_ok());
        }
        assert_eq!(
            inner.push_command_capped(&mut inner.cmd_queue.lock(), Box::new(ListCommands)),
            Err(TmuxEnqueueError::Full)
        );

        let queue = inner.cmd_queue.lock();

        assert_eq!(
            queue.len(),
            CMD_QUEUE_MAX_DEPTH - CMD_QUEUE_TERMINAL_RESERVED_SLOTS
        );
        let head = queue
            .front()
            .expect("in-flight request must remain present");
        let head_text = head.get_command(domain_id);
        assert_eq!(
            head_text, sentinel_cmd_text,
            "overflow rejection must retain the exact in-flight request"
        );
    }

    #[test]
    fn cmd_queue_cap_preserves_oldest_acknowledged_command_when_idle() {
        let default_domain: Arc<dyn Domain> =
            Arc::new(LocalDomain::new("tmux-ft-wajba-idle-default").expect("local domain"));
        let mux = Arc::new(Mux::new(Some(Arc::clone(&default_domain))));
        let _guard = ScopedMux::install(Arc::clone(&mux));

        let launcher = RecordingPane::new(778, default_domain.domain_id());
        let launcher_dyn: Arc<dyn Pane> = launcher.clone();
        mux.add_pane(&launcher_dyn).expect("add launcher pane");

        let tmux_domain = new_tmux_domain(778);
        let inner = Arc::clone(&tmux_domain.inner);
        let domain_id = inner.domain_id;

        let distinctive_head: Box<dyn TmuxCommand> = Box::new(SplitPane {
            pane_id: 8888,
            direction: SplitDirection::Vertical,
            request_id: 2,
        });
        let distinctive_text = distinctive_head.get_command(domain_id);

        {
            let mut queue = inner.cmd_queue.lock();
            assert!(queue.push_back(distinctive_head).is_ok());
        }

        // Keep state as Idle (default after new() is WaitForInitialGuard;
        // set explicitly).
        *inner.state.lock() = State::Idle;

        for _ in 0..CMD_QUEUE_MAX_DEPTH - CMD_QUEUE_TERMINAL_RESERVED_SLOTS - 1 {
            let mut queue = inner.cmd_queue.lock();
            assert!(inner
                .push_command_capped(&mut queue, Box::new(ListCommands))
                .is_ok());
        }
        assert_eq!(
            inner.push_command_capped(&mut inner.cmd_queue.lock(), Box::new(ListCommands)),
            Err(TmuxEnqueueError::Full)
        );

        let queue = inner.cmd_queue.lock();

        assert_eq!(
            queue.len(),
            CMD_QUEUE_MAX_DEPTH - CMD_QUEUE_TERMINAL_RESERVED_SLOTS
        );
        let head = queue
            .front()
            .expect("queue must be non-empty after cap enforcement");
        let head_text = head.get_command(domain_id);
        assert_eq!(
            head_text, distinctive_text,
            "overflow rejection must not discard an acknowledged command"
        );
    }

    #[test]
    fn split_command_error_resolves_pending_split_with_error() {
        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(Arc::clone(&mux));

        let tmux_domain = Arc::new(new_tmux_domain(0));
        let domain: Arc<dyn Domain> = tmux_domain.clone();
        mux.add_domain(&domain)
            .expect("test tmux domain should register");

        let mut promise = promise::Promise::new();
        let future = promise.get_future().expect("split future");
        assert!(tmux_domain
            .inner
            .pending_splits
            .lock()
            .insert(42, promise)
            .is_none());

        let cmd = SplitPane {
            pane_id: 99,
            direction: SplitDirection::Horizontal,
            request_id: 42,
        };
        let result = Guarded {
            error: true,
            timestamp: 0,
            number: 0,
            flags: 0,
            output: "split failed".to_string(),
        };

        let err = cmd
            .process_result(tmux_domain.domain_id(), &result)
            .expect_err("tmux split failure should bubble up");
        assert!(err.to_string().contains("split-window"));

        let future_err = block_on(future).expect_err("pending split should fail closed");
        let future_err = future_err.to_string();
        assert!(future_err.contains("split-window"), "{}", future_err);
        assert!(
            tmux_domain.inner.pending_splits.lock().is_empty(),
            "failed split should consume the pending promise instead of leaving it queued"
        );
    }

    #[test]
    fn terminal_cleanup_fails_pending_split_instead_of_stranding_future() {
        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(Arc::clone(&mux));

        let tmux_domain = Arc::new(new_tmux_domain(0));
        let domain: Arc<dyn Domain> = tmux_domain.clone();
        mux.add_domain(&domain)
            .expect("test tmux domain should register");

        let mut promise = promise::Promise::new();
        let future = promise.get_future().expect("split future");
        assert!(tmux_domain
            .inner
            .pending_splits
            .lock()
            .insert(43, promise)
            .is_none());

        tmux_domain.inner.transition_to_clean_exit();

        let err = block_on(future).expect_err("terminal cleanup must fail the pending split");
        assert!(
            err.to_string().contains("split request 43"),
            "unexpected cancellation error: {:#}",
            err
        );
        assert!(tmux_domain.inner.pending_splits.lock().is_empty());
    }

    #[test]
    fn split_command_results_resolve_exact_request_identity_out_of_order() {
        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(Arc::clone(&mux));

        let tmux_domain = Arc::new(new_tmux_domain(0));
        let domain: Arc<dyn Domain> = tmux_domain.clone();
        mux.add_domain(&domain)
            .expect("test tmux domain should register");

        let mut first_promise = promise::Promise::new();
        let first_future = first_promise.get_future().expect("first split future");
        let mut second_promise = promise::Promise::new();
        let second_future = second_promise.get_future().expect("second split future");
        {
            let mut pending = tmux_domain.inner.pending_splits.lock();
            assert!(pending.insert(100, first_promise).is_none());
            assert!(pending.insert(200, second_promise).is_none());
        }

        let second = SplitPane {
            pane_id: 9,
            direction: SplitDirection::Vertical,
            request_id: 200,
        };
        second
            .process_result(
                tmux_domain.domain_id(),
                &Guarded {
                    error: false,
                    timestamp: 0,
                    number: 0,
                    flags: 0,
                    output: "%22\n".to_string(),
                },
            )
            .expect("second split result");

        let first = SplitPane {
            pane_id: 8,
            direction: SplitDirection::Horizontal,
            request_id: 100,
        };
        first
            .process_result(
                tmux_domain.domain_id(),
                &Guarded {
                    error: false,
                    timestamp: 0,
                    number: 0,
                    flags: 0,
                    output: "%11\n".to_string(),
                },
            )
            .expect("first split result");

        assert_eq!(block_on(first_future).expect("first pane id"), 11);
        assert_eq!(block_on(second_future).expect("second pane id"), 22);
        assert!(tmux_domain.inner.pending_splits.lock().is_empty());
    }
}
