use crate::activity::Activity;
use crate::domain::{alloc_domain_id, Domain, DomainId, DomainState, SplitSource};
use crate::localpane::LocalPane;
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
use lru::LruCache;
use parking_lot::Mutex;
use portable_pty::CommandBuilder;
use std::collections::{HashMap, HashSet, VecDeque};
use std::convert::TryFrom;
use std::fmt;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
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

/// Events received after a guarded command response must wait for that
/// response's main-thread mutation to commit. Bound the barrier so a stalled
/// main thread cannot turn the parser into an unbounded retention path.
const PROTOCOL_BARRIER_MAX_EVENTS: usize = 65_536;
const PROTOCOL_BARRIER_MAX_BYTES: usize = 16 * 1024 * 1024;
const PROTOCOL_BARRIER_DRAIN_EVENT_QUANTUM: usize = 256;
const PROTOCOL_BARRIER_DRAIN_BYTE_QUANTUM: usize = 512 * 1024;

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

#[derive(Debug)]
pub(crate) struct TmuxCmdQueue {
    entries: VecDeque<Box<dyn TmuxCommand>>,
    in_flight: Option<InFlightTmuxCommand>,
    preparing_payload_bytes: Option<usize>,
    payload_bytes: usize,
    closed: bool,
    rejected_commands: u64,
    merged_commands: u64,
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
    remaining_responses: usize,
    first_error: Option<Guarded>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TmuxEnqueueError {
    Closed,
    Full,
}

impl fmt::Display for TmuxEnqueueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => f.write_str("tmux command mailbox is closed"),
            Self::Full => f.write_str("tmux command mailbox is full"),
        }
    }
}

impl std::error::Error for TmuxEnqueueError {}

impl TmuxCmdQueue {
    pub(crate) fn new() -> Self {
        Self {
            entries: VecDeque::new(),
            in_flight: None,
            preparing_payload_bytes: None,
            payload_bytes: 0,
            closed: false,
            rejected_commands: 0,
            merged_commands: 0,
        }
    }

    /// Enqueues only while the owning tmux domain is live. Closing and pushing
    /// share the same mutex, so stale PTY/writer handles cannot refill a queue
    /// after terminal cleanup.
    pub(crate) fn push_back(&mut self, cmd: Box<dyn TmuxCommand>) -> Result<(), TmuxEnqueueError> {
        if self.closed {
            return Err(TmuxEnqueueError::Closed);
        }

        let incoming_payload_bytes = cmd.mailbox_payload_bytes();
        let Some(next_payload_bytes) = self.payload_bytes.checked_add(incoming_payload_bytes)
        else {
            return self.reject_full("payload byte accounting overflow");
        };
        if next_payload_bytes > CMD_QUEUE_MAX_PAYLOAD_BYTES {
            return self.reject_full("payload byte cap");
        }

        if let Some(last) = self.entries.back_mut() {
            if last.try_merge_newer(cmd.as_ref()) {
                self.payload_bytes = next_payload_bytes;
                self.merged_commands = self.merged_commands.saturating_add(1);
                return Ok(());
            }
        }

        if self.len() >= CMD_QUEUE_MAX_DEPTH {
            return self.reject_full("command count cap");
        }

        self.entries.push_back(cmd);
        self.payload_bytes = next_payload_bytes;
        if self.len() == CMD_QUEUE_WARNING_DEPTH.saturating_add(1) {
            log::warn!(
                "tmux command queue depth exceeds {} threshold; possible protocol churn",
                CMD_QUEUE_WARNING_DEPTH
            );
        }
        Ok(())
    }

    /// Admit a required control-plane batch atomically. Required topology
    /// synchronization must never report success after admitting only a
    /// prefix of the batch.
    fn push_required_batch(
        &mut self,
        commands: Vec<Box<dyn TmuxCommand>>,
    ) -> Result<(), TmuxEnqueueError> {
        if self.closed {
            return Err(TmuxEnqueueError::Closed);
        }
        if commands.is_empty() {
            return Ok(());
        }

        let incoming_payload_bytes = commands.iter().try_fold(0usize, |total, command| {
            total.checked_add(command.mailbox_payload_bytes())
        });
        let Some(incoming_payload_bytes) = incoming_payload_bytes else {
            return self.reject_full("required batch payload accounting overflow");
        };
        let Some(next_payload_bytes) = self.payload_bytes.checked_add(incoming_payload_bytes)
        else {
            return self.reject_full("required batch aggregate payload accounting overflow");
        };
        if next_payload_bytes > CMD_QUEUE_MAX_PAYLOAD_BYTES {
            return self.reject_full("required batch payload byte cap");
        }
        let Some(next_depth) = self.len().checked_add(commands.len()) else {
            return self.reject_full("required batch command count accounting overflow");
        };
        if next_depth > CMD_QUEUE_MAX_DEPTH {
            return self.reject_full("required batch command count cap");
        }

        let crossed_warning_threshold =
            self.len() <= CMD_QUEUE_WARNING_DEPTH && next_depth > CMD_QUEUE_WARNING_DEPTH;
        self.entries.extend(commands);
        self.payload_bytes = next_payload_bytes;
        if crossed_warning_threshold {
            log::warn!(
                "tmux command queue depth exceeds {} threshold; possible protocol churn",
                CMD_QUEUE_WARNING_DEPTH
            );
        }
        Ok(())
    }

    fn reject_full(&mut self, reason: &str) -> Result<(), TmuxEnqueueError> {
        self.rejected_commands = self.rejected_commands.saturating_add(1);
        if self.rejected_commands.is_power_of_two() {
            log::error!(
                "tmux command queue rejected work at {reason}; depth={}, payload_bytes={}, \
                 rejected={} in this queue lifetime",
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
        self.preparing_payload_bytes = None;
        self.payload_bytes = 0;
        (std::mem::take(&mut self.entries), self.in_flight.take())
    }

    #[cfg(test)]
    pub(crate) fn is_closed(&self) -> bool {
        self.closed
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
            + usize::from(self.in_flight.is_some())
            + usize::from(self.preparing_payload_bytes.is_some())
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.entries.is_empty()
            && self.in_flight.is_none()
            && self.preparing_payload_bytes.is_none()
    }

    #[cfg(test)]
    pub(crate) fn front(&self) -> Option<&dyn TmuxCommand> {
        self.in_flight
            .as_ref()
            .map(|in_flight| in_flight.command.as_ref())
            .or_else(|| self.entries.front().map(Box::as_ref))
    }

    fn take_next_for_preparation(&mut self) -> Option<Box<dyn TmuxCommand>> {
        debug_assert!(self.preparing_payload_bytes.is_none());
        let command = self.entries.pop_front()?;
        self.preparing_payload_bytes = Some(command.mailbox_payload_bytes());
        Some(command)
    }

    fn release_prepared(&mut self) {
        if let Some(payload_bytes) = self.preparing_payload_bytes.take() {
            self.payload_bytes = self.payload_bytes.saturating_sub(payload_bytes);
        }
    }

    fn install_in_flight(&mut self, cmd: Box<dyn TmuxCommand>) -> bool {
        if self.closed || self.in_flight.is_some() {
            self.release_prepared();
            false
        } else {
            debug_assert_eq!(
                self.preparing_payload_bytes,
                Some(cmd.mailbox_payload_bytes())
            );
            self.preparing_payload_bytes = None;
            let remaining_responses = cmd.expected_responses();
            debug_assert!(remaining_responses > 0);
            self.in_flight = Some(InFlightTmuxCommand {
                command: cmd,
                remaining_responses,
                first_error: None,
            });
            true
        }
    }

    fn record_in_flight_response(
        &mut self,
        response: &Guarded,
    ) -> Option<(Box<dyn TmuxCommand>, Guarded)> {
        let in_flight = self.in_flight.as_mut()?;
        if response.error && in_flight.first_error.is_none() {
            in_flight.first_error = Some(response.clone());
        }
        in_flight.remaining_responses = in_flight.remaining_responses.saturating_sub(1);
        if in_flight.remaining_responses > 0 {
            return None;
        }

        let mut in_flight = self.in_flight.take()?;
        self.payload_bytes = self
            .payload_bytes
            .saturating_sub(in_flight.command.mailbox_payload_bytes());
        let response = in_flight
            .first_error
            .take()
            .unwrap_or_else(|| response.clone());
        Some((in_flight.command, response))
    }

    fn has_pending(&self) -> bool {
        !self.entries.is_empty()
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
    pub cmd_queue: Arc<Mutex<TmuxCmdQueue>>,
    protocol_ingress: Mutex<()>,
    protocol_barrier: Mutex<TmuxProtocolBarrier>,
    pub gui_window: Mutex<Option<MuxWindowBuilder>>,
    pub gui_tabs: Mutex<HashMap<TmuxWindowId, TmuxTab>>,
    pub remote_panes: Mutex<HashMap<TmuxPaneId, RefTmuxRemotePane>>,
    pub(crate) pane_retirement: Mutex<()>,
    pub(crate) retired_panes: Mutex<HashSet<TmuxPaneId>>,
    pub tmux_session: Mutex<Option<TmuxSessionId>>,
    pub support_commands: Mutex<HashMap<String, String>>,
    pub attach_state: Mutex<AttachState>,
    pub notification_sub_id: Mutex<Option<usize>>,
    config_reload_sub: Mutex<Option<config::ConfigSubscription>>,
    backlog_limits_dirty: AtomicBool,
    pending_splits: Mutex<HashMap<u64, promise::Promise<TmuxPaneId>>>,
    next_split_request_id: AtomicU64,
    pub backlog: Mutex<TmuxBacklog>,
}

pub struct TmuxDomain {
    pub(crate) inner: Arc<TmuxDomainState>,
}

#[derive(Debug, Default)]
struct TmuxLifecycle {
    terminal: bool,
    clean_exit: bool,
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
    scheduled: Arc<AtomicBool>,
}

impl Drop for SendScheduleLease {
    fn drop(&mut self) {
        self.scheduled.store(false, Ordering::Release);
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

    fn request_terminal(&self, clean_exit: bool) {
        if clean_exit {
            self.clean_exit_requested.store(true, Ordering::Release);
        }

        let first_transition = {
            let mut lifecycle = self.lifecycle.lock();
            let first_transition = !lifecycle.terminal;
            lifecycle.terminal = true;
            if clean_exit {
                lifecycle.clean_exit = true;
                if lifecycle.detach_disposition == TerminalDetachDisposition::Pending {
                    lifecycle.detach_disposition = TerminalDetachDisposition::NotNeeded;
                }
            } else if first_transition {
                lifecycle.detach_disposition = TerminalDetachDisposition::Pending;
            }

            let mut state = self.state.lock();
            *state = State::Exit;
            first_transition
        };

        // Queue closure and unsubscription are immediate. More expensive
        // resource cleanup is deferred until operations admitted before the
        // terminal transition have drained.
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
        let should_send_detach = {
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

        expected_inner.finalize_launcher_tmux_binding(mux, should_send_detach);
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

    fn finalize_launcher_tmux_binding(&self, mux: &Arc<Mux>, should_send_detach: bool) {
        let Some(pane) = mux.get_pane(self.pane_id) else {
            log::error!(
                "tmux terminal cleanup cannot find launcher pane {} for domain {}",
                self.pane_id,
                self.domain_id
            );
            return;
        };

        if let Some(local_pane) = pane.downcast_ref::<LocalPane>() {
            if should_send_detach {
                match local_pane.send_tmux_detach_if_same(self) {
                    Ok(true) => {}
                    Ok(false) => log::warn!(
                        "tmux terminal cleanup found a replacement launcher binding for domain {}",
                        self.domain_id
                    ),
                    Err(err) => log::error!(
                        "failed to send fail-close detach for tmux domain {}: {err:#}",
                        self.domain_id
                    ),
                }
            }
            let _ = local_pane.clear_tmux_domain_if(self);
            return;
        }

        if should_send_detach {
            log::error!(
                "cannot send exact fail-close detach for tmux domain {} through non-LocalPane \
                 launcher {}",
                self.domain_id,
                self.pane_id
            );
        }
        log::warn!(
            "tmux terminal cleanup found non-LocalPane launcher {}",
            self.pane_id
        );
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
            Ok(()) => {
                Self::schedule_send_next_command(self.domain_id);
                Ok(())
            }
            Err(err) => {
                let error = anyhow::anyhow!(
                    "required tmux command admission failed for domain {} during {context}: {err}",
                    self.domain_id
                );
                if matches!(err, TmuxEnqueueError::Full) {
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
                    }
                    State::WaitingForResponse => {
                        let mut cmd_queue = self.cmd_queue.as_ref().lock();
                        if let Some((cmd, resp)) = cmd_queue.record_in_flight_response(response) {
                            if !self.transition_state(
                                State::WaitingForResponse,
                                State::ProcessingResponse,
                            ) {
                                return None;
                            }
                            drop(cmd_queue);
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
            TmuxDomainState::schedule_send_next_command(self.domain_id);
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
            TmuxDomainState::schedule_send_next_command(self.domain_id);
            return;
        }
    }

    #[cfg(test)]
    fn process_command_result(&self, cmd: Box<dyn TmuxCommand>, response: &Guarded) {
        if !self.apply_command_result(cmd, response) {
            return;
        }
        if self.transition_state(State::ProcessingResponse, State::Idle) {
            TmuxDomainState::schedule_send_next_command(self.domain_id);
        }
    }

    /// send next command at the front of cmd_queue.
    /// must be called inside main thread
    fn send_next_command(&self) {
        if let Err(err) = self.send_next_command_inner() {
            log::error!(
                "failed to transmit a tmux command for domain {}: {err:#}; detaching the domain",
                self.domain_id
            );
            self.transition_to_exit_and_schedule_detach();
        }
    }

    fn send_next_command_inner(&self) -> anyhow::Result<()> {
        let Some(_active_operation) = self.begin_active_operation() else {
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
        };

        {
            let mut cmd_queue = self.cmd_queue.as_ref().lock();
            if !self.transition_state(State::Sending, State::WaitingForResponse) {
                cmd_queue.release_prepared();
                return Ok(());
            }
            if !cmd_queue.install_in_flight(prepared_command) {
                anyhow::bail!(
                    "tmux command mailbox closed or already had an in-flight command during sender reservation"
                );
            }
        }

        log::debug!("sending cmd {command:?}");
        let mux = Mux::try_get().context("active mux disappeared before tmux command write")?;
        let pane = mux.get_pane(self.pane_id).with_context(|| {
            format!(
                "tmux launcher pane {} disappeared before command write",
                self.pane_id
            )
        })?;
        let mut writer = pane.writer();
        write!(writer, "{command}").context("writing command to tmux launcher pane")?;
        Ok(())
    }

    fn should_schedule_send(&self) -> bool {
        let cmd_queue = self.cmd_queue.lock();
        *self.state.lock() == State::Idle && cmd_queue.has_pending()
    }

    fn try_claim_send_schedule(&self) -> Option<SendScheduleLease> {
        self.send_task_scheduled
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;
        Some(SendScheduleLease {
            scheduled: Arc::clone(&self.send_task_scheduled),
        })
    }

    /// Edge-trigger one main-thread sender runnable per tmux domain.
    ///
    /// The runnable releases its scheduling lease before checking for more
    /// work. An enqueue racing that boundary therefore either schedules its
    /// own runnable or is observed by the lost-wakeup recheck below.
    pub fn schedule_send_next_command(domain_id: usize) {
        if !promise::spawn::is_scheduler_configured() {
            return;
        }
        let Some(mux) = Mux::try_get() else {
            return;
        };
        let Some(domain) = mux.get_domain(domain_id) else {
            return;
        };
        let Some(tmux_domain) = domain.downcast_ref::<TmuxDomain>() else {
            return;
        };
        let Some(schedule_lease) = tmux_domain.inner.try_claim_send_schedule() else {
            return;
        };

        let scheduled_inner = Arc::clone(&tmux_domain.inner);
        promise::spawn::spawn_into_main_thread(async move {
            if let Some(mux) = Mux::try_get() {
                if let Some(domain) = mux.get_domain(domain_id) {
                    if let Some(tmux_domain) = domain.downcast_ref::<TmuxDomain>() {
                        if Arc::ptr_eq(&scheduled_inner, &tmux_domain.inner) {
                            tmux_domain.send_next_command();
                        }
                    }
                }
            }

            drop(schedule_lease);
            if scheduled_inner.should_schedule_send() {
                TmuxDomainState::schedule_send_next_command(domain_id);
            }
        })
        .detach();
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
        _tab: TabId,
        pane_id: PaneId,
        split_request: SplitRequest,
        request_id: u64,
    ) -> anyhow::Result<()> {
        let tmux_pane_id = self
            .remote_panes
            .lock()
            .iter()
            .find(|(_, ref_pane)| ref_pane.lock().local_pane_id == pane_id)
            .map(|p| p.1.lock().pane_id);

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
    pub fn new(pane_id: PaneId) -> Self {
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
            cmd_queue: Arc::new(Mutex::new(cmd_queue)),
            protocol_ingress: Mutex::new(()),
            protocol_barrier: Mutex::new(TmuxProtocolBarrier::default()),
            gui_window: Mutex::new(None),
            gui_tabs: Mutex::new(HashMap::default()),
            remote_panes: Mutex::new(HashMap::default()),
            pane_retirement: Mutex::new(()),
            retired_panes: Mutex::new(HashSet::new()),
            tmux_session: Mutex::new(None),
            support_commands: Mutex::new(HashMap::default()),
            attach_state: Mutex::new(AttachState::Init),
            notification_sub_id: Mutex::new(None),
            config_reload_sub: Mutex::new(None),
            backlog_limits_dirty: AtomicBool::new(false),
            pending_splits: Mutex::new(HashMap::default()),
            next_split_request_id: AtomicU64::new(1),
            backlog: Mutex::new(TmuxBacklog::default()),
        });
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
                    .split_tmux_pane(tab, pane_id, split_request, request_id)?;
                anyhow::ensure!(
                    pending_splits.insert(request_id, promise).is_none(),
                    "duplicate tmux split request id {request_id}"
                );
            }
            TmuxDomainState::schedule_send_next_command(self.inner.domain_id);
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
            return self.inner.split_pane(tab, pane_id, id, split_request);
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
        let Some(_detach_operation) = self.inner.begin_active_operation() else {
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

        anyhow::ensure!(
            !self.inner.clean_exit_requested.load(Ordering::Acquire),
            "cannot send detach key after tmux clean exit was requested"
        );
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
    use crate::tab::SplitDirection;
    use crate::tmux_commands::{Resize, SendKeys, SplitPane, TmuxCommand};
    use frankenterm_term::color::ColorPalette;
    use parking_lot::{MappedMutexGuard, MutexGuard};
    use promise::spawn::block_on;
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

        fn shutdown() -> Self {
            let guard = mux_test_lock()
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            let prior = Mux::try_get();
            Mux::shutdown();
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
        writes: Mutex<RecordingWriter>,
        mux_registration: Arc<crate::PaneRegistrationSlot>,
    }

    struct RecordingWriter {
        bytes: Vec<u8>,
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

    impl TmuxCommand for AssertSessionUnsetCommand {
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

    impl TmuxCommand for CountingCommand {
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
    fn tmux_exit_during_in_flight_write_is_nonblocking_and_remains_terminal() {
        let default_domain: Arc<dyn Domain> =
            Arc::new(LocalDomain::new("tmux-send-exit-default").expect("local domain"));
        let mux = Arc::new(Mux::new(Some(Arc::clone(&default_domain))));
        let _guard = ScopedMux::install(Arc::clone(&mux));

        let (launcher, write_entered, release_write) =
            RecordingPane::new_with_blocking_writer(89, default_domain.domain_id());
        let launcher_dyn: Arc<dyn Pane> = launcher.clone();
        mux.add_pane(&launcher_dyn).expect("add launcher pane");

        let tmux_domain = TmuxDomain::new(89);
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

        let sender_inner = Arc::clone(&inner);
        let sender = std::thread::spawn(move || sender_inner.send_next_command());
        if let Err(err) = write_entered.recv_timeout(Duration::from_secs(5)) {
            let _ = release_write.send(());
            sender.join().expect("sender thread should finish");
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
        sender.join().expect("sender thread should finish");
        exit_thread.join().expect("exit thread should finish");

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

        let tmux_domain = Arc::new(TmuxDomain::new(98));
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

        let sender_inner = Arc::clone(&inner);
        let sender = std::thread::spawn(move || sender_inner.send_next_command());
        if let Err(err) = write_entered.recv_timeout(Duration::from_secs(5)) {
            let _ = release_write.send(());
            sender.join().expect("sender thread should finish");
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
        sender.join().expect("sender thread should finish");
        exit_thread.join().expect("exit thread should finish");

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
        let _guard = ScopedMux::install(mux);
        let tmux_domain = TmuxDomain::new(90);
        *tmux_domain.inner.state.lock() = State::Idle;
        assert!(tmux_domain
            .inner
            .cmd_queue
            .lock()
            .push_back(Box::new(ListCommands))
            .is_ok());

        tmux_domain.inner.send_next_command();

        assert_eq!(*tmux_domain.inner.state.lock(), State::Exit);
        assert!(
            tmux_domain.inner.cmd_queue.lock().is_empty(),
            "terminal cleanup must clear an unsendable command"
        );
    }

    #[test]
    fn tmux_command_send_fails_closed_when_mux_disappears() {
        let _guard = ScopedMux::shutdown();
        let tmux_domain = TmuxDomain::new(91);
        *tmux_domain.inner.state.lock() = State::Idle;
        assert!(tmux_domain
            .inner
            .cmd_queue
            .lock()
            .push_back(Box::new(ListCommands))
            .is_ok());

        tmux_domain.inner.send_next_command();

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

        let tmux_domain = TmuxDomain::new(92);
        *tmux_domain.inner.state.lock() = State::Idle;
        assert!(tmux_domain
            .inner
            .cmd_queue
            .lock()
            .push_back(Box::new(ListCommands))
            .is_ok());

        tmux_domain.inner.send_next_command();

        assert_eq!(*tmux_domain.inner.state.lock(), State::Exit);
        let queue = tmux_domain.inner.cmd_queue.lock();
        assert!(queue.is_closed());
        assert!(queue.is_empty());
    }

    #[test]
    fn tmux_terminal_queue_rejects_stale_producers_and_session_events() {
        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(mux);
        let tmux_domain = TmuxDomain::new(93);

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
        let tmux_domain = TmuxDomain::new(94);
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
        inner.cmd_queue.lock().in_flight = Some(InFlightTmuxCommand {
            command: Box::new(AssertSessionUnsetCommand {
                owner: Arc::downgrade(inner),
                processed,
            }),
            remaining_responses: 1,
            first_error: None,
        });
        *inner.state.lock() = State::WaitingForResponse;
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
        let tmux_domain = Arc::new(TmuxDomain::new(97));
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
        let tmux_domain = Arc::new(TmuxDomain::new(98));
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
        let tmux_domain = Arc::new(TmuxDomain::new(99));
        let domain: Arc<dyn Domain> = tmux_domain.clone();
        mux.add_domain(&domain).expect("register tmux test domain");
        let processed = Arc::new(AtomicUsize::new(0));
        tmux_domain.inner.cmd_queue.lock().in_flight = Some(InFlightTmuxCommand {
            command: Box::new(CountingCommand {
                processed: Arc::clone(&processed),
            }),
            remaining_responses: 1,
            first_error: None,
        });
        *tmux_domain.inner.state.lock() = State::WaitingForResponse;

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
        let tmux_domain = Arc::new(TmuxDomain::new(95));
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
        let tmux_domain = Arc::new(TmuxDomain::new(96));
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
        let tmux_domain = TmuxDomain::new(97);
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
        let tmux_domain = TmuxDomain::new(98);
        let first = tmux_domain
            .inner
            .try_claim_send_schedule()
            .expect("first runnable must claim the scheduling edge");
        assert!(
            tmux_domain.inner.try_claim_send_schedule().is_none(),
            "a producer burst must not allocate a second sender runnable"
        );
        drop(first);
        assert!(
            tmux_domain.inner.try_claim_send_schedule().is_some(),
            "dropping or cancelling the runnable must rearm scheduling"
        );
    }

    #[test]
    fn tmux_domain_state_reports_detached_after_exit() {
        let tmux_domain = TmuxDomain::new(0);
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

        let tmux_domain = TmuxDomain::new(99);
        // Every producer shares this mailbox, so it must enforce the cap even
        // if the sender is stalled and never reaches a pop site.
        {
            let mut queue = tmux_domain.inner.cmd_queue.lock();
            for _ in 0..CMD_QUEUE_MAX_DEPTH {
                assert!(queue.push_back(Box::new(ListCommands)).is_ok());
            }
            for _ in 0..100 {
                assert_eq!(
                    queue.push_back(Box::new(ListCommands)),
                    Err(TmuxEnqueueError::Full)
                );
            }
            assert_eq!(queue.len(), CMD_QUEUE_MAX_DEPTH);
            assert_eq!(queue.rejected_commands, 100);
        }
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
        for _ in 0..CMD_QUEUE_MAX_DEPTH - 1 {
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
        queue
            .push_back(Box::new(ListCommands))
            .expect("following command should be admitted");
        let resize = queue
            .take_next_for_preparation()
            .expect("resize should enter preparation");
        assert!(queue.install_in_flight(resize));

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
        let (completed, retained_response) = queue
            .record_in_flight_response(&second_success)
            .expect("the second response should complete the resize");
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

        let tmux_domain = TmuxDomain::new(777);
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
            assert!(queue.install_in_flight(prepared));
            assert_eq!(queue.len(), 1);
        }

        *inner.state.lock() = State::WaitingForResponse;

        for _ in 0..CMD_QUEUE_MAX_DEPTH - 1 {
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

        assert_eq!(queue.len(), CMD_QUEUE_MAX_DEPTH);
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

        let tmux_domain = TmuxDomain::new(778);
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

        for _ in 0..CMD_QUEUE_MAX_DEPTH - 1 {
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

        assert_eq!(queue.len(), CMD_QUEUE_MAX_DEPTH);
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

        let tmux_domain = Arc::new(TmuxDomain::new(0));
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

        let tmux_domain = Arc::new(TmuxDomain::new(0));
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

        let tmux_domain = Arc::new(TmuxDomain::new(0));
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
