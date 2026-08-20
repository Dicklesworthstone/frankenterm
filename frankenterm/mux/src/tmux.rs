use crate::activity::Activity;
use crate::domain::{Domain, DomainId, DomainState, alloc_domain_id};
use crate::localpane::LocalPane;
use crate::pane::{Pane, PaneId};
use crate::tab::{SplitRequest, Tab, TabId};
use crate::tmux_commands::{
    CompensateSplitPane, DetachClient, ListAllPanes, ListAllWindows, ListCommands,
    ReconcileSplitPane, SnapshotSplitPane, SplitPane, SplitPaneIdentityParse, TmuxCommand,
    TmuxCommandClass, TmuxCommandPreparation, TmuxConditionalCommit, TmuxConditionalCommitIntent,
    TmuxConditionalCommitLease, TmuxConditionalCommitTarget, TmuxPreparationPrerequisite,
    TmuxSplitFailureAuthority, parse_split_pane_identity,
};
use crate::tmux_pty::TmuxChildState;
use crate::window::WindowId;
use crate::{
    DomainOperationGuard, Mux, MuxWindowBuilder, PaneOperationGuard, SplitCommitReceipt,
    TopologyRevision,
};
use anyhow::Context;
use async_trait::async_trait;
use config::configuration;
use crossbeam::channel::{Receiver, Sender, TrySendError, bounded};
use filedescriptor::FileDescriptor;
use frankenterm_sigpipe::{RecoverablePanicSite, catch_recoverable};
use frankenterm_term::TerminalSize;
use lru::LruCache;
use parking_lot::Mutex;
use portable_pty::CommandBuilder;
use std::cell::Cell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::convert::TryFrom;
use std::fmt;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};
use termwiz::tmux_cc::*;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct TmuxBacklogLimits {
    per_pane_bytes: usize,
    total_bytes: usize,
    entries: usize,
    items: usize,
    expiry: Duration,
}

impl TmuxBacklogLimits {
    pub(crate) fn current() -> Self {
        let config = configuration();
        Self {
            per_pane_bytes: config.mux_tmux_max_backlog_bytes_per_pane,
            total_bytes: config.mux_tmux_max_backlog_bytes,
            entries: config.mux_tmux_max_backlog_entries,
            items: config.mux_tmux_max_backlog_items,
            expiry: Duration::from_millis(config.mux_tmux_backlog_expiry_ms),
        }
    }

    #[cfg(test)]
    pub(crate) fn new(per_pane_bytes: usize, total_bytes: usize, entries: usize) -> Self {
        Self {
            per_pane_bytes,
            total_bytes,
            entries,
            items: usize::MAX,
            expiry: Duration::MAX,
        }
    }

    #[cfg(test)]
    fn with_item_expiry(
        per_pane_bytes: usize,
        total_bytes: usize,
        entries: usize,
        items: usize,
        expiry: Duration,
    ) -> Self {
        Self {
            per_pane_bytes,
            total_bytes,
            entries,
            items,
            expiry,
        }
    }

    fn disables_retention(self) -> bool {
        self.per_pane_bytes == 0
            || self.total_bytes == 0
            || self.entries == 0
            || self.items == 0
            || self.expiry.is_zero()
    }
}

#[derive(Debug)]
struct PaneBacklog {
    chunks: VecDeque<Vec<u8>>,
    byte_len: usize,
    updated_at: Instant,
    gapped: bool,
}

impl PaneBacklog {
    fn new(updated_at: Instant) -> Self {
        Self {
            chunks: VecDeque::new(),
            byte_len: 0,
            updated_at,
            gapped: false,
        }
    }

    fn clear_payload(&mut self) -> (usize, usize) {
        let bytes = std::mem::take(&mut self.byte_len);
        let items = self.chunks.len();
        self.chunks = VecDeque::new();
        (bytes, items)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TmuxBacklogDrain {
    Bytes(VecDeque<Vec<u8>>),
    ResyncRequired,
}

#[derive(Debug)]
pub(crate) struct TmuxBacklog {
    entries: LruCache<TmuxPaneId, PaneBacklog>,
    total_bytes: usize,
    total_items: usize,
    gapped_entries: usize,
    limits: TmuxBacklogLimits,
    resync_all: bool,
    dropped_bytes: u64,
    evicted_entries: u64,
    expired_entries: u64,
    reported_dropped_bytes: u64,
    reported_evicted_entries: u64,
    reported_expired_entries: u64,
}

impl Default for TmuxBacklog {
    fn default() -> Self {
        Self {
            entries: LruCache::unbounded(),
            total_bytes: 0,
            total_items: 0,
            gapped_entries: 0,
            limits: TmuxBacklogLimits::default(),
            resync_all: false,
            dropped_bytes: 0,
            evicted_entries: 0,
            expired_entries: 0,
            reported_dropped_bytes: 0,
            reported_evicted_entries: 0,
            reported_expired_entries: 0,
        }
    }
}

impl TmuxBacklog {
    pub(crate) fn append_owned_with_limits(
        &mut self,
        pane_id: TmuxPaneId,
        payload: Vec<u8>,
        limits: TmuxBacklogLimits,
    ) {
        self.append_owned_with_limits_at(pane_id, payload, limits, Instant::now());
    }

    fn append_owned_with_limits_at(
        &mut self,
        pane_id: TmuxPaneId,
        payload: Vec<u8>,
        limits: TmuxBacklogLimits,
        now: Instant,
    ) {
        self.refresh_limits_at(limits, now);
        if payload.is_empty() {
            self.record_metrics();
            return;
        }
        if limits.disables_retention() {
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
        let mut entry = self
            .entries
            .pop(&pane_id)
            .unwrap_or_else(|| PaneBacklog::new(now));
        let old_len = entry.byte_len;
        let old_items = entry.chunks.len();
        self.total_bytes = self.total_bytes.saturating_sub(old_len);
        self.total_items = self.total_items.saturating_sub(old_items);

        if entry.gapped {
            self.dropped_bytes = self
                .dropped_bytes
                .saturating_add(u64::try_from(payload.len()).unwrap_or(u64::MAX));
            entry.clear_payload();
        } else if old_len
            .checked_add(payload.len())
            .is_none_or(|combined| combined > pane_cap)
        {
            // Never retain an arbitrary terminal-stream suffix. It could begin
            // inside UTF-8, CSI, OSC, or another stateful escape sequence.
            // Preserve a bounded marker and require an authoritative capture.
            let dropped = old_len.saturating_add(payload.len());
            self.dropped_bytes = self
                .dropped_bytes
                .saturating_add(u64::try_from(dropped).unwrap_or(u64::MAX));
            entry.clear_payload();
            entry.gapped = true;
            self.gapped_entries = self.gapped_entries.saturating_add(1);
        } else {
            entry.byte_len += payload.len();
            entry.chunks.push_back(payload);
        }
        entry.updated_at = now;
        self.total_bytes = self.total_bytes.saturating_add(entry.byte_len);
        self.total_items = self.total_items.saturating_add(entry.chunks.len());
        self.entries.put(pane_id, entry);

        self.enforce_entry_and_aggregate_limits();
        self.record_metrics();
    }

    pub(crate) fn refresh_limits(&mut self, limits: TmuxBacklogLimits) {
        self.refresh_limits_at(limits, Instant::now());
    }

    fn refresh_limits_at(&mut self, limits: TmuxBacklogLimits, now: Instant) {
        let limits_changed = self.limits != limits;
        self.limits = limits;
        if limits.disables_retention() {
            self.mark_global_resync();
            self.record_metrics();
            return;
        }
        if self.resync_all {
            self.record_metrics();
            return;
        }

        // Appends pop and reinsert their pane, so LRU order is also
        // last-update order. The overwhelmingly common no-expiry path needs
        // only the oldest entry check rather than a scan of every unknown pane.
        let oldest_expired = self.entries.iter().next_back().is_some_and(|(_, entry)| {
            now.saturating_duration_since(entry.updated_at) >= limits.expiry
        });
        if oldest_expired {
            let expired = self
                .entries
                .iter()
                .filter(|(_, entry)| {
                    now.saturating_duration_since(entry.updated_at) >= limits.expiry
                })
                .count();
            self.expired_entries = self
                .expired_entries
                .saturating_add(u64::try_from(expired).unwrap_or(u64::MAX));
            self.mark_global_resync();
            self.record_metrics();
            return;
        }

        // Per-pane entries already satisfied the previous limits. Only a hot
        // contraction requires a full reconciliation scan; normal output
        // admission enforces aggregate limits after adding its one new chunk.
        if limits_changed {
            let pane_cap = limits.per_pane_bytes.min(limits.total_bytes);
            let pane_ids = self
                .entries
                .iter()
                .map(|(pane_id, _)| *pane_id)
                .collect::<Vec<_>>();
            for pane_id in pane_ids {
                let Some(entry) = self.entries.peek(&pane_id) else {
                    continue;
                };
                if entry.byte_len > pane_cap {
                    self.gap_entry(pane_id);
                }
            }
            self.enforce_entry_and_aggregate_limits();
        }
        self.record_metrics();
    }

    fn gap_entry(&mut self, pane_id: TmuxPaneId) {
        let Some(mut entry) = self.entries.pop(&pane_id) else {
            return;
        };
        let (dropped, dropped_items) = entry.clear_payload();
        if !entry.gapped {
            entry.gapped = true;
            self.gapped_entries = self.gapped_entries.saturating_add(1);
        }
        self.total_bytes = self.total_bytes.saturating_sub(dropped);
        self.total_items = self.total_items.saturating_sub(dropped_items);
        self.dropped_bytes = self
            .dropped_bytes
            .saturating_add(u64::try_from(dropped).unwrap_or(u64::MAX));
        self.entries.put(pane_id, entry);
    }

    fn enforce_entry_and_aggregate_limits(&mut self) {
        if self.entries.len() > self.limits.entries {
            let (_pane_id, mut entry) = self
                .entries
                .pop_lru()
                .expect("tmux backlog entry count exceeded with no entry");
            let (dropped, dropped_items) = entry.clear_payload();
            self.total_bytes = self.total_bytes.saturating_sub(dropped);
            self.total_items = self.total_items.saturating_sub(dropped_items);
            self.dropped_bytes = self
                .dropped_bytes
                .saturating_add(u64::try_from(dropped).unwrap_or(u64::MAX));
            self.evicted_entries = self.evicted_entries.saturating_add(1);
            // Losing the marker itself means we no longer know which pane is
            // incomplete. The only fail-closed bounded representation is one
            // global resynchronization requirement.
            self.mark_global_resync();
            return;
        }

        while self.total_bytes > self.limits.total_bytes || self.total_items > self.limits.items {
            let Some(pane_id) = self
                .entries
                .iter()
                .filter(|(_, entry)| !entry.chunks.is_empty())
                .map(|(pane_id, _)| *pane_id)
                .next_back()
            else {
                self.total_bytes = 0;
                self.total_items = 0;
                break;
            };
            self.gap_entry(pane_id);
        }
    }

    pub(crate) fn take(&mut self, pane_id: TmuxPaneId) -> Option<TmuxBacklogDrain> {
        let entry = self.entries.pop(&pane_id)?;
        self.total_bytes = self.total_bytes.saturating_sub(entry.byte_len);
        self.total_items = self.total_items.saturating_sub(entry.chunks.len());
        if entry.gapped {
            self.gapped_entries = self.gapped_entries.saturating_sub(1);
        }
        self.record_metrics();
        if entry.gapped {
            Some(TmuxBacklogDrain::ResyncRequired)
        } else {
            Some(TmuxBacklogDrain::Bytes(entry.chunks))
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
        self.resync_all || self.gapped_entries != 0
    }

    pub(crate) fn remove(&mut self, pane_id: TmuxPaneId) -> bool {
        let Some(entry) = self.entries.pop(&pane_id) else {
            return false;
        };
        self.total_bytes = self.total_bytes.saturating_sub(entry.byte_len);
        self.total_items = self.total_items.saturating_sub(entry.chunks.len());
        if entry.gapped {
            self.gapped_entries = self.gapped_entries.saturating_sub(1);
        }
        self.record_metrics();
        true
    }

    pub(crate) fn remove_many(&mut self, pane_ids: &[TmuxPaneId]) {
        for pane_id in pane_ids {
            let _ = self.remove(*pane_id);
        }
    }

    fn extend_pane_id_snapshot(&self, snapshot: &mut HashSet<TmuxPaneId>) {
        snapshot.extend(self.entries.iter().map(|(pane_id, _)| *pane_id));
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
        self.total_items = 0;
        self.gapped_entries = 0;
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
        let newly_expired = self
            .expired_entries
            .saturating_sub(self.reported_expired_entries);
        if newly_expired > 0 {
            metrics::counter!("mux.tmux.backlog.expired_entries").increment(newly_expired);
            self.reported_expired_entries = self.expired_entries;
        }
        metrics::histogram!("mux.tmux.backlog.entries").record(self.entries.len() as f64);
        metrics::histogram!("mux.tmux.backlog.items").record(self.total_items as f64);
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
    fn total_items(&self) -> usize {
        self.total_items
    }

    #[cfg(test)]
    fn pane_bytes(&self, pane_id: TmuxPaneId) -> Option<Vec<u8>> {
        self.entries.peek(&pane_id).map(|entry| {
            entry
                .chunks
                .iter()
                .flat_map(|chunk| chunk.iter().copied())
                .collect()
        })
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
            .map(|(_, entry)| {
                entry
                    .chunks
                    .iter()
                    .map(Vec::capacity)
                    .fold(0_usize, usize::saturating_add)
            })
            .fold(0_usize, usize::saturating_add)
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

/// Once a parked intent becomes ready, bound how many freshly admitted
/// intents may overtake it. This is independent of durable-lane fairness:
/// neither continuous key/control traffic nor a resize/focus storm can starve
/// the exact target whose prerequisite was just published.
const CMD_QUEUE_RETRY_INTENT_SERVICE_QUANTUM: usize = 32;

/// Bound side-effect-free command preparation on the main thread. A mailbox
/// can retain tens of thousands of entries; suppressed, stale, or temporarily
/// blocked commands must yield instead of monopolizing an event-loop turn.
const CMD_QUEUE_PREPARATION_QUANTUM: usize = 64;

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

/// Retain a bounded causal record when malformed control output makes an
/// abandoned split identity ambiguous.  Ambiguity is never permission to kill
/// a pane that may predate the request.
const TMUX_SPLIT_QUARANTINE_LIMIT: usize = 1_024;

/// A scoped window census larger than this is rejected before `split-window`
/// can run.  The fixed bound lets each pending request own response storage
/// without allocation at the effect/rollback boundary.
const TMUX_SPLIT_REMOTE_BASELINE_LIMIT: usize = 16_384;

/// Bound the aggregate fixed-capacity baseline reservations retained by
/// concurrent split callers.
const TMUX_PENDING_SPLIT_LIMIT: usize = 64;

fn parse_scoped_split_pane_line(
    line: &str,
) -> anyhow::Result<(TmuxSessionId, TmuxWindowId, TmuxPaneId, &str)> {
    let mut fields = line.trim_end().splitn(4, ' ');
    let session = fields
        .next()
        .and_then(|field| field.strip_prefix('$'))
        .context("scoped tmux pane row lacks a session id")?
        .parse::<TmuxSessionId>()
        .context("scoped tmux pane row has an invalid session id")?;
    let window = fields
        .next()
        .and_then(|field| field.strip_prefix('@'))
        .context("scoped tmux pane row lacks a window id")?
        .parse::<TmuxWindowId>()
        .context("scoped tmux pane row has an invalid window id")?;
    let pane = fields
        .next()
        .and_then(|field| field.strip_prefix('%'))
        .context("scoped tmux pane row lacks a pane id")?
        .parse::<TmuxPaneId>()
        .context("scoped tmux pane row has an invalid pane id")?;
    Ok((session, window, pane, fields.next().unwrap_or("").trim()))
}

/// Bound single-flight pane IDs retained by one domain output lane. This is
/// deliberately independent of the much smaller unknown-pane backlog
/// cardinality: a large, fully materialized agent fleet must not detach merely
/// because more than 1024 panes become writable in one burst.
const TMUX_OUTPUT_ACTIVE_PANE_LIMIT: usize = 16_384;

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
    /// ordered ingress queue and makes textual recovery unsafe.
    Captured,
    /// The initial stream/capture and cursor transaction committed; live
    /// output may now enter the nonblocking drain lane.
    Ready,
    /// The remote pane was detached. Any producer that retained the pane gate
    /// must discard late output rather than resurrecting a backlog entry.
    Retired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TmuxPaneOutputLimits {
    bytes_per_pane: usize,
    items_per_pane: usize,
    write_quantum_bytes: usize,
}

impl TmuxPaneOutputLimits {
    pub(crate) fn current() -> Self {
        let config = configuration();
        Self {
            bytes_per_pane: config.mux_tmux_max_backlog_bytes_per_pane,
            items_per_pane: config.mux_tmux_max_output_queue_items_per_pane,
            write_quantum_bytes: config.mux_tmux_output_write_quantum_bytes,
        }
    }

    #[cfg(test)]
    fn new(bytes_per_pane: usize, items_per_pane: usize, write_quantum_bytes: usize) -> Self {
        Self {
            bytes_per_pane,
            items_per_pane,
            write_quantum_bytes,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TmuxPaneOutputGap {
    ByteLimit,
    ItemLimit,
    ArithmeticOverflow,
    BacklogRecoveryRequired,
    DrainLaneCapacity,
    DrainLaneClosed,
    InvalidQuantum,
    InvalidState,
}

impl TmuxPaneOutputGap {
    fn label(self) -> &'static str {
        match self {
            Self::ByteLimit => "byte_limit",
            Self::ItemLimit => "item_limit",
            Self::ArithmeticOverflow => "arithmetic_overflow",
            Self::BacklogRecoveryRequired => "backlog_recovery_required",
            Self::DrainLaneCapacity => "drain_lane_capacity",
            Self::DrainLaneClosed => "drain_lane_closed",
            Self::InvalidQuantum => "invalid_quantum",
            Self::InvalidState => "invalid_state",
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct TmuxPaneOutputIngress {
    chunks: VecDeque<Vec<u8>>,
    front_offset: usize,
    queued_bytes: usize,
    drain_scheduled: bool,
    capture_raced: bool,
}

impl TmuxPaneOutputIngress {
    fn validate_addition(
        &self,
        added_bytes: usize,
        added_items: usize,
        limits: TmuxPaneOutputLimits,
    ) -> Result<(), TmuxPaneOutputGap> {
        let bytes = self
            .queued_bytes
            .checked_add(added_bytes)
            .ok_or(TmuxPaneOutputGap::ArithmeticOverflow)?;
        if bytes > limits.bytes_per_pane {
            return Err(TmuxPaneOutputGap::ByteLimit);
        }
        let items = self
            .chunks
            .len()
            .checked_add(added_items)
            .ok_or(TmuxPaneOutputGap::ArithmeticOverflow)?;
        if items > limits.items_per_pane {
            return Err(TmuxPaneOutputGap::ItemLimit);
        }
        Ok(())
    }

    pub(crate) fn push_back(
        &mut self,
        payload: Vec<u8>,
        limits: TmuxPaneOutputLimits,
    ) -> Result<(), TmuxPaneOutputGap> {
        if payload.is_empty() {
            return Ok(());
        }
        self.validate_addition(payload.len(), 1, limits)?;
        self.queued_bytes += payload.len();
        self.chunks.push_back(payload);
        Ok(())
    }

    pub(crate) fn prepend(
        &mut self,
        mut chunks: VecDeque<Vec<u8>>,
        limits: TmuxPaneOutputLimits,
    ) -> Result<(), TmuxPaneOutputGap> {
        if self.front_offset != 0 || self.drain_scheduled {
            return Err(TmuxPaneOutputGap::InvalidState);
        }
        let added_bytes = chunks
            .iter()
            .try_fold(0_usize, |total, chunk| total.checked_add(chunk.len()))
            .ok_or(TmuxPaneOutputGap::ArithmeticOverflow)?;
        self.validate_addition(added_bytes, chunks.len(), limits)?;
        chunks.append(&mut self.chunks);
        self.chunks = chunks;
        self.queued_bytes += added_bytes;
        Ok(())
    }

    pub(crate) fn clear(&mut self) {
        self.chunks = VecDeque::new();
        self.front_offset = 0;
        self.queued_bytes = 0;
        self.drain_scheduled = false;
        self.capture_raced = false;
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    pub(crate) fn queued_bytes(&self) -> usize {
        self.queued_bytes
    }

    pub(crate) fn capture_raced(&self) -> bool {
        self.capture_raced
    }
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
    pub(crate) output_ingress: TmuxPaneOutputIngress,
}

pub(crate) type RefTmuxRemotePane = Arc<Mutex<TmuxRemotePane>>;

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TmuxRemoteSplitState {
    Reserved = 0,
    Published = 1,
    Retired = 2,
}

impl TmuxRemoteSplitState {
    fn from_raw(raw: u8) -> Option<Self> {
        match raw {
            0 => Some(Self::Reserved),
            1 => Some(Self::Published),
            2 => Some(Self::Retired),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub(crate) struct TmuxRemoteSplitStateCell {
    state: AtomicU8,
}

impl TmuxRemoteSplitStateCell {
    fn new() -> Self {
        Self {
            state: AtomicU8::new(TmuxRemoteSplitState::Reserved as u8),
        }
    }

    pub(crate) fn load(&self) -> anyhow::Result<TmuxRemoteSplitState> {
        let raw = self.state.load(Ordering::Acquire);
        TmuxRemoteSplitState::from_raw(raw)
            .ok_or_else(|| anyhow::anyhow!("invalid tmux remote split state {raw}"))
    }

    pub(crate) fn transition(
        &self,
        expected: TmuxRemoteSplitState,
        next: TmuxRemoteSplitState,
    ) -> anyhow::Result<()> {
        self.state
            .compare_exchange(
                expected as u8,
                next as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map(|_| ())
            .map_err(|observed| {
                anyhow::anyhow!(
                    "tmux remote split state changed from {expected:?} to {:?} before {next:?}",
                    TmuxRemoteSplitState::from_raw(observed)
                )
            })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TmuxSplitCleanupStatus {
    Prepared,
    Armed,
    Published,
    Claimed,
    Completed,
    Failed,
}

const TMUX_SPLIT_KILL_COMMAND_CAPACITY: usize = 64;

#[derive(Debug)]
struct TmuxSplitCleanupState {
    status: TmuxSplitCleanupStatus,
    pane_id: Option<TmuxPaneId>,
    kill_command: Option<Vec<u8>>,
}

/// Exact, one-shot authority to compensate a remotely-created split pane.
///
/// Its state and fixed-capacity kill buffer are allocated before `split-window`
/// can mutate tmux.  The mutex is also the final publication fence: local
/// topology publication retains an `Armed` guard through its structural cut,
/// while terminalization and reservation drop must acquire the same guard
/// before claiming the already-prepared kill.
#[derive(Debug)]
pub(crate) struct TmuxSplitCleanupObligation {
    owner: Weak<TmuxDomainState>,
    request_id: u64,
    child_state: Arc<TmuxChildState>,
    state: Mutex<TmuxSplitCleanupState>,
}

impl TmuxSplitCleanupObligation {
    fn new(
        owner: &Arc<TmuxDomainState>,
        request_id: u64,
        child_state: Arc<TmuxChildState>,
    ) -> anyhow::Result<Arc<Self>> {
        let mut kill_command = Vec::new();
        kill_command
            .try_reserve_exact(TMUX_SPLIT_KILL_COMMAND_CAPACITY)
            .map_err(|error| anyhow::anyhow!("reserve tmux split kill command: {error}"))?;
        Ok(Arc::new(Self {
            owner: Arc::downgrade(owner),
            request_id,
            child_state,
            state: Mutex::new(TmuxSplitCleanupState {
                status: TmuxSplitCleanupStatus::Prepared,
                pane_id: None,
                kill_command: Some(kill_command),
            }),
        }))
    }

    fn install_remote_identity(&self, pane_id: TmuxPaneId) -> anyhow::Result<()> {
        let mut state = self.state.lock();
        anyhow::ensure!(
            state.status == TmuxSplitCleanupStatus::Prepared && state.pane_id.is_none(),
            "tmux split cleanup request {} changed before remote identity installation",
            self.request_id
        );
        let command = state
            .kill_command
            .as_mut()
            .context("tmux split cleanup lost its preallocated kill command")?;
        command.clear();
        command.extend_from_slice(b"kill-pane -t %");
        let mut digits = [0_u8; 20];
        let mut remaining = pane_id;
        let mut cursor = digits.len();
        loop {
            cursor -= 1;
            digits[cursor] = b'0' + u8::try_from(remaining % 10).unwrap_or(0);
            remaining /= 10;
            if remaining == 0 {
                break;
            }
        }
        command.extend_from_slice(&digits[cursor..]);
        command.push(b'\n');
        anyhow::ensure!(
            command.len() <= TMUX_SPLIT_KILL_COMMAND_CAPACITY,
            "tmux split kill command exceeded its preallocated capacity"
        );
        state.pane_id = Some(pane_id);
        state.status = TmuxSplitCleanupStatus::Armed;
        Ok(())
    }

    pub(crate) fn begin_publication(&self) -> anyhow::Result<TmuxSplitCleanupPublication<'_>> {
        let state = self.state.lock();
        anyhow::ensure!(
            state.status == TmuxSplitCleanupStatus::Armed,
            "tmux split cleanup request {} changed to {:?} before local publication",
            self.request_id,
            state.status
        );
        Ok(TmuxSplitCleanupPublication { state })
    }

    fn claim(&self) -> bool {
        let mut state = self.state.lock();
        if !matches!(
            state.status,
            TmuxSplitCleanupStatus::Armed | TmuxSplitCleanupStatus::Published
        ) {
            return false;
        }
        state.status = TmuxSplitCleanupStatus::Claimed;
        true
    }

    fn claim_published_child(
        &self,
        pane_id: TmuxPaneId,
        child_state: &Arc<TmuxChildState>,
    ) -> bool {
        let mut state = self.state.lock();
        if state.status != TmuxSplitCleanupStatus::Published
            || state.pane_id != Some(pane_id)
            || !Arc::ptr_eq(&self.child_state, child_state)
        {
            return false;
        }
        state.status = TmuxSplitCleanupStatus::Claimed;
        true
    }

    fn complete_callback_drain(&self) {
        {
            let mut state = self.state.lock();
            if state.status != TmuxSplitCleanupStatus::Published {
                return;
            }
            state.status = TmuxSplitCleanupStatus::Completed;
        }
        if let Some(owner) = self.owner.upgrade() {
            owner.finish_split_cleanup_obligation(
                self,
                true,
                "tmux split callback drain completed",
            );
        }
    }

    fn complete_without_remote_effect(&self, reason: &'static str) {
        {
            let mut state = self.state.lock();
            if state.status != TmuxSplitCleanupStatus::Prepared {
                return;
            }
            state.status = TmuxSplitCleanupStatus::Completed;
        }
        self.child_state
            .mark_exited(portable_pty::ExitStatus::with_signal(reason));
        if let Some(owner) = self.owner.upgrade() {
            owner.finish_split_cleanup_obligation(self, true, reason);
        }
    }

    fn fail_without_remote_identity(&self, reason: &'static str) {
        {
            let mut state = self.state.lock();
            if state.status != TmuxSplitCleanupStatus::Prepared {
                return;
            }
            state.status = TmuxSplitCleanupStatus::Failed;
        }
        self.child_state
            .mark_exited(portable_pty::ExitStatus::with_signal(reason));
        if let Some(owner) = self.owner.upgrade() {
            owner.finish_split_cleanup_obligation(self, false, reason);
        }
    }

    pub(crate) fn take_kill_command(&self) -> anyhow::Result<Vec<u8>> {
        let mut state = self.state.lock();
        anyhow::ensure!(
            state.status == TmuxSplitCleanupStatus::Claimed,
            "tmux split cleanup request {} is not claimed",
            self.request_id
        );
        state
            .kill_command
            .take()
            .context("tmux split cleanup lost its immutable kill command")
    }

    pub(crate) fn pane_id(&self) -> Option<TmuxPaneId> {
        self.state.lock().pane_id
    }

    pub(crate) fn request_id(&self) -> u64 {
        self.request_id
    }

    pub(crate) fn finish_claimed(&self, succeeded: bool, reason: &'static str) {
        let next = if succeeded {
            TmuxSplitCleanupStatus::Completed
        } else {
            TmuxSplitCleanupStatus::Failed
        };
        {
            let mut state = self.state.lock();
            if state.status != TmuxSplitCleanupStatus::Claimed {
                log::error!(
                    "tmux split cleanup request {} completed from unexpected state {:?}",
                    self.request_id,
                    state.status
                );
                return;
            }
            state.status = next;
        }
        self.child_state
            .mark_exited(portable_pty::ExitStatus::with_signal(reason));
        if let Some(owner) = self.owner.upgrade() {
            owner.finish_split_cleanup_obligation(self, succeeded, reason);
        }
    }

    fn status(&self) -> TmuxSplitCleanupStatus {
        self.state.lock().status
    }
}

pub(crate) struct TmuxSplitCleanupPublication<'a> {
    state: parking_lot::MutexGuard<'a, TmuxSplitCleanupState>,
}

impl TmuxSplitCleanupPublication<'_> {
    fn complete(mut self) {
        debug_assert_eq!(self.state.status, TmuxSplitCleanupStatus::Armed);
        self.state.status = TmuxSplitCleanupStatus::Published;
        // Publication completion is the callback-free structural cut.  Never
        // retain the status fence while map retirement, scheduling, or any mux
        // subscriber can re-enter this domain.
        drop(self.state);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TmuxSplitQuarantine {
    request_id: u64,
    candidates: Vec<TmuxPaneId>,
    reason: String,
}

#[cfg(test)]
pub(crate) const TEST_SPLIT_OUTPUT_LANE_FULL: u8 = 1;
#[cfg(test)]
pub(crate) const TEST_SPLIT_OUTPUT_LANE_CLOSED: u8 = 2;
#[cfg(test)]
pub(crate) const TEST_SPLIT_OUTPUT_STATE_RACE: u8 = 3;

/// One capacity unit reserved from the bounded pane-output lane before a
/// pane becomes structurally visible. Dropping an unpublished reservation
/// releases only its own unit; sending it after the local commit cannot report
/// transaction failure.
#[derive(Debug)]
struct TmuxPaneOutputReservation {
    ready: Sender<TmuxPaneId>,
    active: Arc<AtomicUsize>,
    pane_id: TmuxPaneId,
    armed: bool,
}

impl TmuxPaneOutputReservation {
    fn send(mut self) -> Result<(), TmuxPaneOutputGap> {
        let result = match self.ready.try_send(self.pane_id) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(TmuxPaneOutputGap::DrainLaneCapacity),
            Err(TrySendError::Disconnected(_)) => Err(TmuxPaneOutputGap::DrainLaneClosed),
        };
        if result.is_err() {
            self.active.fetch_sub(1, Ordering::AcqRel);
        }
        self.armed = false;
        result
    }
}

impl Drop for TmuxPaneOutputReservation {
    fn drop(&mut self) {
        if self.armed {
            self.active.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

struct PendingTmuxSplit {
    promise: promise::Promise<TmuxRemoteSplitReservation>,
    target_remote_pane_id: TmuxPaneId,
    target_session_id: TmuxSessionId,
    target_window_id: TmuxWindowId,
    request_token: String,
    child_state: Arc<TmuxChildState>,
    state: Arc<TmuxRemoteSplitStateCell>,
    cleanup: Arc<TmuxSplitCleanupObligation>,
    identity_permit: Option<TmuxRetainedPaneIdentityPermit>,
    baseline_remote_pane_ids: Vec<TmuxPaneId>,
    split_command: Option<Box<SplitPane>>,
    reconcile_command: Option<Box<ReconcileSplitPane>>,
    baseline_complete: bool,
    reconciling: bool,
}

impl PendingTmuxSplit {
    fn new(
        owner: &Arc<TmuxDomainState>,
        request_id: u64,
        promise: promise::Promise<TmuxRemoteSplitReservation>,
        target_remote_pane_id: TmuxPaneId,
        target_session_id: TmuxSessionId,
        target_window_id: TmuxWindowId,
        direction: crate::tab::SplitDirection,
    ) -> anyhow::Result<Self> {
        let child_state = Arc::new(TmuxChildState::new());
        let (identity_permit, cleanup, baseline_remote_pane_ids) =
            owner.reserve_remote_split_identity(request_id, Arc::clone(&child_state))?;
        let request_token = format!("ft-{}-{request_id}", owner.domain_id);
        Ok(Self {
            promise,
            target_remote_pane_id,
            target_session_id,
            target_window_id,
            request_token,
            child_state,
            state: Arc::new(TmuxRemoteSplitStateCell::new()),
            cleanup,
            identity_permit: Some(identity_permit),
            baseline_remote_pane_ids,
            split_command: Some(Box::new(SplitPane::new(
                owner.domain_id,
                target_remote_pane_id,
                direction,
                request_id,
            ))),
            reconcile_command: Some(Box::new(ReconcileSplitPane::new(
                request_id,
                target_remote_pane_id,
                target_window_id,
            ))),
            baseline_complete: false,
            reconciling: false,
        })
    }

    #[cfg(test)]
    fn new_test(
        owner: &Arc<TmuxDomainState>,
        request_id: u64,
        promise: promise::Promise<TmuxRemoteSplitReservation>,
        target_remote_pane_id: TmuxPaneId,
    ) -> anyhow::Result<Self> {
        let mut pending = Self::new(
            owner,
            request_id,
            promise,
            target_remote_pane_id,
            1,
            1,
            crate::tab::SplitDirection::Horizontal,
        )?;
        pending.baseline_remote_pane_ids.push(target_remote_pane_id);
        pending.baseline_complete = true;
        if let Err(error) = owner.cmd_queue.lock().reserve_split_cleanup(Box::new(
            CompensateSplitPane {
                obligation: Arc::clone(&pending.cleanup),
            },
        )) {
            pending.cleanup.complete_without_remote_effect(
                "test split cleanup admission failed before remote effect",
            );
            return Err(anyhow::anyhow!(
                "reserve test split cleanup slot for request {request_id}: {error}"
            ));
        }
        Ok(pending)
    }
}

/// One retained-identity slot reserved before `split-window` can become
/// externally visible. The response consumes the permit into an exact remote
/// pane tombstone; every earlier failure releases it without mutating tmux.
#[derive(Debug)]
struct TmuxRetainedPaneIdentityPermit {
    owner: Weak<TmuxDomainState>,
    armed: bool,
}

impl TmuxRetainedPaneIdentityPermit {
    fn validate_locked(&self, owner: &Arc<TmuxDomainState>) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.armed && Weak::ptr_eq(&self.owner, &Arc::downgrade(owner)),
            "tmux retained-pane identity permit belongs to another domain generation"
        );
        anyhow::ensure!(
            owner.remote_split_identity_permits.load(Ordering::Acquire) != 0,
            "tmux retained-pane identity permit accounting underflow"
        );
        Ok(())
    }

    fn consume_locked(mut self, owner: &Arc<TmuxDomainState>) {
        debug_assert!(self.validate_locked(owner).is_ok());
        let prior = owner
            .remote_split_identity_permits
            .fetch_sub(1, Ordering::AcqRel);
        debug_assert_ne!(prior, 0);
        self.armed = false;
    }
}

impl Drop for TmuxRetainedPaneIdentityPermit {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Some(owner) = self.owner.upgrade() else {
            return;
        };
        if owner
            .remote_split_identity_permits
            .try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_sub(1)
            })
            .is_err()
        {
            log::error!(
                "tmux domain {} retained-pane identity permit accounting underflow",
                owner.domain_id
            );
        }
        self.armed = false;
    }
}

/// Cancellation-owning authority for one remote pane created by tmux.
///
/// A successful `split-window` response publishes this reservation before the
/// protocol barrier releases any following pane output. Until local mux
/// registration, structural ownership, pane counts, lifecycle notification,
/// and reader gate commit together, dropping the token retires the remote
/// identity and compensates it with exactly one `kill-pane` request.
pub(crate) struct TmuxRemoteSplitReservation {
    owner: Arc<TmuxDomainState>,
    request_id: u64,
    target_remote_pane_id: TmuxPaneId,
    remote_pane_id: TmuxPaneId,
    child_state: Arc<TmuxChildState>,
    state: Arc<TmuxRemoteSplitStateCell>,
    cleanup: Arc<TmuxSplitCleanupObligation>,
    published_gate: Option<RefTmuxRemotePane>,
    published_local_pane_id: Option<PaneId>,
    published_window_id: Option<TmuxWindowId>,
    output_reservation: Option<TmuxPaneOutputReservation>,
    armed: bool,
}

impl fmt::Debug for TmuxRemoteSplitReservation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TmuxRemoteSplitReservation")
            .field("request_id", &self.request_id)
            .field("target_remote_pane_id", &self.target_remote_pane_id)
            .field("remote_pane_id", &self.remote_pane_id)
            .field("published_local_pane_id", &self.published_local_pane_id)
            .field("published_window_id", &self.published_window_id)
            .field("output_prepared", &self.output_reservation.is_some())
            .field("cleanup_status", &self.cleanup.state.lock().status)
            .field("armed", &self.armed)
            .finish_non_exhaustive()
    }
}

impl TmuxRemoteSplitReservation {
    pub(crate) fn remote_pane_id(&self) -> TmuxPaneId {
        self.remote_pane_id
    }

    pub(crate) fn target_remote_pane_id(&self) -> TmuxPaneId {
        self.target_remote_pane_id
    }

    pub(crate) fn child_state(&self) -> Arc<TmuxChildState> {
        Arc::clone(&self.child_state)
    }

    pub(crate) fn cleanup_obligation(&self) -> Arc<TmuxSplitCleanupObligation> {
        Arc::clone(&self.cleanup)
    }

    pub(crate) fn publish_local_mirror(
        &mut self,
        remote_gate: RefTmuxRemotePane,
        local_pane_id: PaneId,
        remote_window_id: TmuxWindowId,
    ) -> anyhow::Result<()> {
        self.published_gate = Some(remote_gate);
        self.published_local_pane_id = Some(local_pane_id);
        self.published_window_id = Some(remote_window_id);
        let owner = Arc::clone(&self.owner);
        owner.publish_reserved_remote_split(self)
    }

    /// Reserve the bounded output-lane unit and freeze the pane's first drain
    /// edge before any local mux topology becomes visible.
    pub(crate) fn prepare_output_commit(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(self.armed, "tmux split reservation is no longer live");
        anyhow::ensure!(
            self.output_reservation.is_none(),
            "tmux split output was prepared more than once"
        );
        anyhow::ensure!(
            self.state.load()? == TmuxRemoteSplitState::Published,
            "tmux split pane {} lost published mirror authority before output preflight",
            self.remote_pane_id
        );
        let remote_gate = self
            .published_gate
            .as_ref()
            .cloned()
            .context("published tmux split lacks its output gate")?;

        #[cfg(test)]
        for (stage, gap) in [
            (
                TEST_SPLIT_OUTPUT_LANE_FULL,
                TmuxPaneOutputGap::DrainLaneCapacity,
            ),
            (
                TEST_SPLIT_OUTPUT_LANE_CLOSED,
                TmuxPaneOutputGap::DrainLaneClosed,
            ),
        ] {
            if self
                .owner
                .test_split_output_failure
                .compare_exchange(stage, 0, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                anyhow::bail!(
                    "tmux split pane {} output preflight failed: {}",
                    self.remote_pane_id,
                    gap.label()
                );
            }
        }

        let output_reservation = self
            .owner
            .output_lane
            .get()
            .ok_or(TmuxPaneOutputGap::DrainLaneClosed)
            .and_then(|lane| lane.reserve(self.remote_pane_id))
            .map_err(|gap| {
                anyhow::anyhow!(
                    "tmux split pane {} output preflight failed: {}",
                    self.remote_pane_id,
                    gap.label()
                )
            })?;
        let mut remote = remote_gate.lock();
        #[cfg(test)]
        if self
            .owner
            .test_split_output_failure
            .compare_exchange(
                TEST_SPLIT_OUTPUT_STATE_RACE,
                0,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            remote.output_state = TmuxPaneOutputState::Ready;
        }
        anyhow::ensure!(
            remote.output_state == TmuxPaneOutputState::Fresh
                && !remote.output_ingress.drain_scheduled,
            "tmux remote split {} output gate changed to {:?} before local commit",
            self.remote_pane_id,
            remote.output_state
        );
        remote.output_ingress.drain_scheduled = true;
        remote.output_state = TmuxPaneOutputState::Ready;
        drop(remote);
        self.output_reservation = Some(output_reservation);
        Ok(())
    }

    /// Consume the already-reserved output edge after the local structural
    /// commit. A vanishing worker at this point is an absorbing domain failure,
    /// not a transaction error: the caller receives its committed receipt and
    /// exact-domain teardown removes the complete published topology.
    pub(crate) fn complete_structural_cut(&mut self, publication: TmuxSplitCleanupPublication<'_>) {
        publication.complete();
        self.armed = false;
    }

    pub(crate) fn finish_committed_output(mut self) {
        self.cleanup.complete_callback_drain();
        let output_reservation = self.output_reservation.take();
        debug_assert!(!self.armed);
        let Some(output_reservation) = output_reservation else {
            log::error!(
                "tmux remote split {} reached local commit without reserved output authority; \
                 detaching the committed domain",
                self.remote_pane_id
            );
            self.owner.transition_to_exit_and_schedule_detach();
            return;
        };
        if let Err(gap) = output_reservation.send() {
            log::error!(
                "tmux remote split {} lost its reserved output lane after local commit ({}); \
                 detaching the committed domain",
                self.remote_pane_id,
                gap.label()
            );
            self.owner.transition_to_exit_and_schedule_detach();
        } else {
            metrics::counter!("mux.tmux.output.drain_scheduled").increment(1);
        }
    }
}

impl Drop for TmuxRemoteSplitReservation {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.owner.rollback_remote_split(self);
    }
}

#[derive(Debug)]
struct TmuxPaneOutputLane {
    ready: Sender<TmuxPaneId>,
    active: Arc<AtomicUsize>,
    connected: Arc<AtomicBool>,
    capacity: usize,
}

impl TmuxPaneOutputLane {
    fn new(
        domain_id: DomainId,
        owner: Weak<TmuxDomainState>,
        capacity: usize,
    ) -> anyhow::Result<Self> {
        let (ready_tx, ready_rx) = bounded(capacity.max(1));
        let active = Arc::new(AtomicUsize::new(0));
        let worker_active = Arc::clone(&active);
        let connected = Arc::new(AtomicBool::new(true));
        let worker_connected = Arc::clone(&connected);
        let panic_owner = owner.clone();
        std::thread::Builder::new()
            .name(format!("tmux-output-{domain_id}"))
            .spawn(move || {
                let outcome = catch_recoverable(
                    RecoverablePanicSite::MuxTmuxCallback,
                    std::panic::AssertUnwindSafe(|| {
                        run_tmux_pane_output_lane(owner, ready_rx, &worker_active);
                    }),
                );
                worker_connected.store(false, Ordering::Release);
                if outcome.is_err() {
                    if let Some(owner) = panic_owner.upgrade() {
                        log::error!(
                            "tmux domain {} output lane panicked; detaching",
                            owner.domain_id
                        );
                        owner.transition_to_exit_and_schedule_detach();
                    }
                }
            })
            .with_context(|| {
                format!("cannot start bounded tmux pane-output lane for domain {domain_id}")
            })?;
        Ok(Self {
            ready: ready_tx,
            active,
            connected,
            capacity,
        })
    }

    fn reserve(&self, pane_id: TmuxPaneId) -> Result<TmuxPaneOutputReservation, TmuxPaneOutputGap> {
        if !self.connected.load(Ordering::Acquire) {
            return Err(TmuxPaneOutputGap::DrainLaneClosed);
        }
        let mut active = self.active.load(Ordering::Acquire);
        loop {
            if active >= self.capacity {
                return Err(TmuxPaneOutputGap::DrainLaneCapacity);
            }
            let Some(next) = active.checked_add(1) else {
                return Err(TmuxPaneOutputGap::DrainLaneCapacity);
            };
            match self.active.compare_exchange_weak(
                active,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(observed) => active = observed,
            }
        }
        if !self.connected.load(Ordering::Acquire) {
            self.active.fetch_sub(1, Ordering::AcqRel);
            return Err(TmuxPaneOutputGap::DrainLaneClosed);
        }
        Ok(TmuxPaneOutputReservation {
            ready: self.ready.clone(),
            active: Arc::clone(&self.active),
            pane_id,
            armed: true,
        })
    }

    fn schedule(&self, pane_id: TmuxPaneId) -> Result<(), TmuxPaneOutputGap> {
        self.reserve(pane_id)?.send()
    }
}

fn alloc_nonwrapping_atomic_u64(counter: &AtomicU64) -> Option<u64> {
    let mut current = counter.load(Ordering::Acquire);
    loop {
        let next = current.checked_add(1)?;
        match counter.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(allocated) => return Some(allocated),
            Err(observed) => current = observed,
        }
    }
}

enum TmuxPaneDrainDisposition {
    Complete,
    Ready,
    Blocked,
    Failed(std::io::Error),
    Gap(TmuxPaneOutputGap),
}

fn run_tmux_pane_output_lane(
    owner: Weak<TmuxDomainState>,
    ready_rx: Receiver<TmuxPaneId>,
    active: &AtomicUsize,
) {
    const BLOCKED_RETRY_QUANTA: usize = 64;
    const BLOCKED_RETRY_BATCH: usize = 64;
    const BLOCKED_RETRY_DELAY: Duration = Duration::from_millis(2);

    let mut ready = VecDeque::new();
    let mut blocked = VecDeque::new();
    let mut ready_quanta_since_blocked_retry = 0_usize;
    loop {
        let Some(domain) = owner.upgrade() else {
            return;
        };
        if domain.is_terminal() {
            return;
        }

        while let Ok(pane_id) = ready_rx.try_recv() {
            ready.push_back(pane_id);
        }

        if !blocked.is_empty() && ready_quanta_since_blocked_retry >= BLOCKED_RETRY_QUANTA {
            if let Some(pane_id) = blocked.pop_front() {
                ready.push_front(pane_id);
                ready_quanta_since_blocked_retry = 0;
            }
        }

        if ready.is_empty() {
            if blocked.is_empty() {
                match ready_rx.recv_timeout(Duration::from_millis(10)) {
                    Ok(pane_id) => ready.push_back(pane_id),
                    Err(crossbeam::channel::RecvTimeoutError::Timeout) => continue,
                    Err(crossbeam::channel::RecvTimeoutError::Disconnected) => return,
                }
            } else {
                match ready_rx.recv_timeout(BLOCKED_RETRY_DELAY) {
                    Ok(pane_id) => {
                        ready.push_back(pane_id);
                        continue;
                    }
                    Err(crossbeam::channel::RecvTimeoutError::Timeout) => {
                        let retry_count = blocked.len().min(BLOCKED_RETRY_BATCH);
                        for _ in 0..retry_count {
                            if let Some(pane_id) = blocked.pop_front() {
                                ready.push_back(pane_id);
                            }
                        }
                        ready_quanta_since_blocked_retry = 0;
                    }
                    Err(crossbeam::channel::RecvTimeoutError::Disconnected) => return,
                }
            }
        }

        let Some(pane_id) = ready.pop_front() else {
            continue;
        };
        match domain.drain_pane_output_quantum(pane_id) {
            TmuxPaneDrainDisposition::Complete => {
                active.fetch_sub(1, Ordering::AcqRel);
            }
            TmuxPaneDrainDisposition::Ready => ready.push_back(pane_id),
            TmuxPaneDrainDisposition::Blocked => blocked.push_back(pane_id),
            TmuxPaneDrainDisposition::Failed(err) => {
                active.fetch_sub(1, Ordering::AcqRel);
                log::error!(
                    "tmux pane {pane_id} output failed in domain {}: {err}",
                    domain.domain_id
                );
                domain.transition_to_exit_and_schedule_detach();
                return;
            }
            TmuxPaneDrainDisposition::Gap(gap) => {
                active.fetch_sub(1, Ordering::AcqRel);
                domain.fail_pane_output_gap(pane_id, gap);
                return;
            }
        }
        if blocked.is_empty() {
            ready_quanta_since_blocked_retry = 0;
        } else {
            ready_quanta_since_blocked_retry = ready_quanta_since_blocked_retry.saturating_add(1);
        }
    }
}

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
    pub(crate) fn prepare_pane_registration(
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
        self.pane_by_local.try_reserve(1).map_err(|error| {
            anyhow::anyhow!("reserve local tmux pane reverse-index entry: {error}")
        })?;
        self.pane_by_remote.try_reserve(1).map_err(|error| {
            anyhow::anyhow!("reserve remote tmux pane reverse-index entry: {error}")
        })?;
        Ok(())
    }

    pub(crate) fn commit_pane_registration(
        &mut self,
        local_pane_id: PaneId,
        remote_pane_id: TmuxPaneId,
    ) {
        let prior_local = self.pane_by_local.insert(local_pane_id, remote_pane_id);
        let prior_remote = self.pane_by_remote.insert(remote_pane_id, local_pane_id);
        debug_assert!(prior_local.is_none() && prior_remote.is_none());
    }

    #[cfg(test)]
    pub(crate) fn register_test_pane(
        &mut self,
        local_pane_id: PaneId,
        remote_pane_id: TmuxPaneId,
    ) -> anyhow::Result<()> {
        self.prepare_pane_registration(local_pane_id, remote_pane_id)?;
        self.commit_pane_registration(local_pane_id, remote_pane_id);
        Ok(())
    }

    pub(crate) fn unregister_pane(
        &mut self,
        remote_pane_id: TmuxPaneId,
    ) -> anyhow::Result<Option<PaneId>> {
        let Some(local_pane_id) = self.checked_local_pane_for_remote(remote_pane_id)? else {
            return Ok(None);
        };
        let _ = self.pane_by_remote.remove(&remote_pane_id);
        let _ = self.pane_by_local.remove(&local_pane_id);
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
        let Some(local_tab_id) = self.tab_by_remote_window.get(&remote_window_id).copied() else {
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
            self.window_by_local_tab.get(&local_tab_id) == Some(&remote_window_id),
            "tmux window reverse index disagrees for local tab {local_tab_id} and remote window \
             {remote_window_id}"
        );
        let _ = self.tab_by_remote_window.remove(&remote_window_id);
        let _ = self.window_by_local_tab.remove(&local_tab_id);
        Ok(Some(local_tab_id))
    }

    pub(crate) fn remote_pane_for_local(&self, local_pane_id: PaneId) -> Option<TmuxPaneId> {
        self.pane_by_local.get(&local_pane_id).copied()
    }

    pub(crate) fn checked_local_pane_for_remote(
        &self,
        remote_pane_id: TmuxPaneId,
    ) -> anyhow::Result<Option<PaneId>> {
        let Some(local_pane_id) = self.pane_by_remote.get(&remote_pane_id).copied() else {
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
            self.pane_by_local.get(&local_pane_id) == Some(&remote_pane_id),
            "tmux pane reverse index disagrees for local pane {local_pane_id} and remote pane \
             {remote_pane_id}"
        );
        Ok(Some(local_pane_id))
    }

    pub(crate) fn remote_window_for_local_tab(&self, local_tab_id: TabId) -> Option<TmuxWindowId> {
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

    pub(crate) fn take_ordered_batch(&mut self) -> [Option<SequencedTmuxNotificationIntent>; 2] {
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
            superseded = superseded.saturating_add(u64::from(!self.restore_if_current(remaining)));
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
            && (self.pending_pane_focus.is_some() || self.pending_window_invalidation.is_some())
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
    split_cleanup_entries: VecDeque<TmuxSplitCleanupSlot>,
    split_transaction_entries: VecDeque<Box<dyn TmuxCommand>>,
    durable_entries: VecDeque<Box<dyn TmuxCommand>>,
    intent_entries: VecDeque<Box<dyn TmuxCommand>>,
    retry_deferred_durable: VecDeque<DeferredTmuxCommand>,
    retry_deferred_intent_head: Option<TmuxConditionalCommitTarget>,
    retry_deferred_intent_tail: Option<TmuxConditionalCommitTarget>,
    retry_deferred_intents: HashMap<TmuxConditionalCommitTarget, DeferredTmuxCommand>,
    ready_retry_deferred_intents: VecDeque<TmuxConditionalCommitTarget>,
    in_flight: Option<InFlightTmuxCommand>,
    preparing: Option<PreparingTmuxCommand>,
    retained_by_class: [usize; TmuxCommandClass::COUNT],
    durable_since_intent: usize,
    intents_since_retry_deferred_durable: usize,
    fresh_intents_since_retry_deferred_intent: usize,
    payload_bytes: usize,
    closed: bool,
    terminal_barrier: bool,
    split_cleanup_barrier: bool,
    terminal_command_dispatched: bool,
    rejected_commands: u64,
    merged_commands: u64,
    next_conditional_commit_generation: u64,
    latest_conditional_commits: HashMap<TmuxConditionalCommitTarget, TmuxConditionalCommitLease>,
    queued_conditional_commits:
        HashMap<TmuxConditionalCommitTarget, VecDeque<TmuxConditionalCommitLease>>,
    uncertain_remote_pane_sizes: HashSet<TmuxPaneId>,
}

#[derive(Debug)]
struct TmuxSplitCleanupSlot {
    command: Box<dyn TmuxCommand>,
    ready: bool,
}

impl TmuxSplitCleanupSlot {
    fn command(&self) -> &dyn TmuxCommand {
        self.command.as_ref()
    }

    fn is_ready(&self) -> bool {
        self.ready
    }

    fn into_command(self) -> Box<dyn TmuxCommand> {
        self.command
    }
}

#[derive(Debug)]
struct PreparingTmuxCommand {
    class: TmuxCommandClass,
    payload_bytes: usize,
    conditional_commit: Option<TmuxConditionalCommitLease>,
    superseded: bool,
    split_transaction: bool,
    split_failure_authority: Option<TmuxSplitFailureAuthority>,
}

#[derive(Debug)]
struct DeferredTmuxCommand {
    command: Box<dyn TmuxCommand>,
    conditional_commit: Option<TmuxConditionalCommitLease>,
    retry_prerequisite: TmuxPreparationPrerequisite,
    retry_ready: bool,
    retry_intent_previous: Option<TmuxConditionalCommitTarget>,
    retry_intent_next: Option<TmuxConditionalCommitTarget>,
}

/// Owns every allocation detached from a closed mailbox. The terminal path
/// drops this value only after releasing the mailbox mutex, so command
/// destructors, hash buckets, and high-water deque allocations cannot extend
/// producer lock hold time.
#[derive(Debug)]
pub(crate) struct TmuxCmdQueueTeardown {
    split_cleanup_entries: VecDeque<TmuxSplitCleanupSlot>,
    split_transaction_entries: VecDeque<Box<dyn TmuxCommand>>,
    durable_entries: VecDeque<Box<dyn TmuxCommand>>,
    intent_entries: VecDeque<Box<dyn TmuxCommand>>,
    retry_deferred_durable: VecDeque<DeferredTmuxCommand>,
    ready_retry_deferred_intents: VecDeque<TmuxConditionalCommitTarget>,
    retry_deferred_intents: HashMap<TmuxConditionalCommitTarget, DeferredTmuxCommand>,
    in_flight: Option<InFlightTmuxCommand>,
    preparing: Option<PreparingTmuxCommand>,
    latest_conditional_commits: HashMap<TmuxConditionalCommitTarget, TmuxConditionalCommitLease>,
    queued_conditional_commits:
        HashMap<TmuxConditionalCommitTarget, VecDeque<TmuxConditionalCommitLease>>,
    uncertain_remote_pane_sizes: HashSet<TmuxPaneId>,
}

impl Drop for TmuxCmdQueueTeardown {
    fn drop(&mut self) {
        // Explicitly release retained values before the collection allocations
        // themselves. This method runs outside the queue critical section.
        self.split_cleanup_entries.clear();
        self.split_transaction_entries.clear();
        self.durable_entries.clear();
        self.intent_entries.clear();
        self.retry_deferred_durable.clear();
        self.ready_retry_deferred_intents.clear();
        self.retry_deferred_intents.clear();
        drop(self.in_flight.take());
        drop(self.preparing.take());
        self.latest_conditional_commits.clear();
        self.queued_conditional_commits.clear();
        self.uncertain_remote_pane_sizes.clear();
    }
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
    conditional_commit: Option<TmuxConditionalCommit>,
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
            Self::SchedulerUnavailable => f.write_str("the main-thread scheduler is unavailable"),
            Self::MuxUnavailable => f.write_str("the active mux is unavailable"),
            Self::DomainUnavailable => f.write_str("the tmux domain is not registered"),
            Self::WrongDomainType => f.write_str("the registered domain is not a tmux domain"),
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
            split_cleanup_entries: VecDeque::new(),
            split_transaction_entries: VecDeque::new(),
            durable_entries: VecDeque::new(),
            intent_entries: VecDeque::new(),
            retry_deferred_durable: VecDeque::new(),
            retry_deferred_intent_head: None,
            retry_deferred_intent_tail: None,
            retry_deferred_intents: HashMap::new(),
            ready_retry_deferred_intents: VecDeque::new(),
            in_flight: None,
            preparing: None,
            retained_by_class: [0; TmuxCommandClass::COUNT],
            durable_since_intent: 0,
            intents_since_retry_deferred_durable: 0,
            fresh_intents_since_retry_deferred_intent: 0,
            payload_bytes: 0,
            closed: false,
            terminal_barrier: false,
            split_cleanup_barrier: false,
            terminal_command_dispatched: false,
            rejected_commands: 0,
            merged_commands: 0,
            next_conditional_commit_generation: 1,
            latest_conditional_commits: HashMap::new(),
            queued_conditional_commits: HashMap::new(),
            uncertain_remote_pane_sizes: HashSet::new(),
        }
    }

    fn allocate_conditional_commit_lease(
        &mut self,
        intent: Option<TmuxConditionalCommitIntent>,
        class: TmuxCommandClass,
    ) -> Result<Option<TmuxConditionalCommitLease>, TmuxEnqueueError> {
        let Some(intent) = intent else {
            return Ok(None);
        };
        let generation = self.next_conditional_commit_generation;
        let Some(next_generation) = generation.checked_add(1) else {
            return self
                .reject_full(class, "conditional commit generation exhausted")
                .map(|()| None);
        };
        self.next_conditional_commit_generation = next_generation;
        Ok(Some(TmuxConditionalCommitLease { generation, intent }))
    }

    pub(crate) fn advance_preparation_prerequisite(
        &mut self,
        prerequisite: TmuxPreparationPrerequisite,
    ) {
        for deferred in &mut self.retry_deferred_durable {
            if deferred.retry_prerequisite == prerequisite {
                deferred.retry_ready = true;
            }
        }

        match prerequisite {
            TmuxPreparationPrerequisite::Attach => {
                // Attach is the sole domain-wide wake. Traverse the explicit
                // dormant insertion order once so targets made ready together
                // retain deterministic FIFO service.
                let mut target = self.retry_deferred_intent_head;
                while let Some(current_target) = target {
                    let deferred = self
                        .retry_deferred_intents
                        .get_mut(&current_target)
                        .expect("tmux deferred-intent order lost its indexed entry");
                    target = deferred.retry_intent_next;
                    if deferred.retry_prerequisite == prerequisite && !deferred.retry_ready {
                        deferred.retry_ready = true;
                        self.ready_retry_deferred_intents.push_back(current_target);
                    }
                }
            }
            TmuxPreparationPrerequisite::Pane(pane_id) => {
                let target = TmuxConditionalCommitTarget::PaneSize(pane_id);
                if let Some(deferred) = self.retry_deferred_intents.get_mut(&target) {
                    if deferred.retry_prerequisite == prerequisite && !deferred.retry_ready {
                        deferred.retry_ready = true;
                        self.ready_retry_deferred_intents.push_back(target);
                    }
                }
            }
        }
    }

    fn publish_conditional_commit_lease(&mut self, lease: Option<TmuxConditionalCommitLease>) {
        if let Some(lease) = lease {
            self.latest_conditional_commits
                .insert(lease.intent.target(), lease);
        }
    }

    fn register_queued_conditional_commit(&mut self, lease: TmuxConditionalCommitLease) {
        self.queued_conditional_commits
            .entry(lease.intent.target())
            .or_default()
            .push_back(lease);
    }

    fn replace_last_queued_conditional_commit(
        &mut self,
        lease: TmuxConditionalCommitLease,
    ) -> bool {
        let Some(last) = self
            .queued_conditional_commits
            .get_mut(&lease.intent.target())
            .and_then(|leases| leases.back_mut())
        else {
            return false;
        };
        *last = lease;
        true
    }

    fn take_queued_conditional_commit(
        &mut self,
        target: TmuxConditionalCommitTarget,
    ) -> Option<TmuxConditionalCommitLease> {
        let (lease, empty) = {
            let queue = self.queued_conditional_commits.get_mut(&target)?;
            let lease = queue.pop_front();
            (lease, queue.is_empty())
        };
        if empty {
            self.queued_conditional_commits.remove(&target);
        }
        lease
    }

    pub(crate) fn conditional_commit_is_current(&self, lease: &TmuxConditionalCommitLease) -> bool {
        self.latest_conditional_commits.get(&lease.intent.target()) == Some(lease)
    }

    fn retire_conditional_commit_if_current(&mut self, lease: &TmuxConditionalCommitLease) -> bool {
        if !self.conditional_commit_is_current(lease) {
            return false;
        }
        self.latest_conditional_commits
            .remove(&lease.intent.target());
        true
    }

    /// Cached pane dimensions are suppression authority only while no resize
    /// for this remote pane may have reached tmux without an exact matching
    /// success being committed locally.
    pub(crate) fn pane_size_suppression_is_trustworthy(&self, pane_id: TmuxPaneId) -> bool {
        !self.uncertain_remote_pane_sizes.contains(&pane_id)
    }

    /// Definitive pane retirement is the only non-success path allowed to
    /// discard remote-size uncertainty. The pane identity no longer exists,
    /// so no future request can suppress against its cached dimensions.
    pub(crate) fn retire_pane_size_suppression_target(&mut self, pane_id: TmuxPaneId) {
        self.uncertain_remote_pane_sizes.remove(&pane_id);
    }

    fn append_retry_deferred_intent(
        &mut self,
        target: TmuxConditionalCommitTarget,
        mut deferred: DeferredTmuxCommand,
    ) {
        debug_assert!(!self.retry_deferred_intents.contains_key(&target));
        let previous = self.retry_deferred_intent_tail;
        deferred.retry_intent_previous = previous;
        deferred.retry_intent_next = None;
        let replaced = self.retry_deferred_intents.insert(target, deferred);
        debug_assert!(replaced.is_none());

        if let Some(previous) = previous {
            self.retry_deferred_intents
                .get_mut(&previous)
                .expect("tmux deferred-intent tail lost its indexed entry")
                .retry_intent_next = Some(target);
        } else {
            debug_assert!(self.retry_deferred_intent_head.is_none());
            self.retry_deferred_intent_head = Some(target);
        }
        self.retry_deferred_intent_tail = Some(target);
    }

    fn remove_retry_deferred_intent(
        &mut self,
        target: TmuxConditionalCommitTarget,
    ) -> Option<DeferredTmuxCommand> {
        let deferred = self.retry_deferred_intents.remove(&target)?;
        match deferred.retry_intent_previous {
            Some(previous) => {
                self.retry_deferred_intents
                    .get_mut(&previous)
                    .expect("tmux deferred-intent predecessor lost its indexed entry")
                    .retry_intent_next = deferred.retry_intent_next;
            }
            None => self.retry_deferred_intent_head = deferred.retry_intent_next,
        }
        match deferred.retry_intent_next {
            Some(next) => {
                self.retry_deferred_intents
                    .get_mut(&next)
                    .expect("tmux deferred-intent successor lost its indexed entry")
                    .retry_intent_previous = deferred.retry_intent_previous;
            }
            None => self.retry_deferred_intent_tail = deferred.retry_intent_previous,
        }
        Some(deferred)
    }

    /// Enqueues only while the owning tmux domain is live. Closing and pushing
    /// share the same mutex, so stale PTY/writer handles cannot refill a queue
    /// after terminal cleanup.
    pub(crate) fn push_back(&mut self, cmd: Box<dyn TmuxCommand>) -> Result<(), TmuxEnqueueError> {
        if self.closed || self.terminal_barrier || self.split_cleanup_barrier {
            return Err(TmuxEnqueueError::Closed);
        }

        let class = cmd.mailbox_class();
        let conditional_commit =
            self.allocate_conditional_commit_lease(cmd.conditional_commit_intent(), class)?;
        let incoming_payload_bytes = cmd.mailbox_payload_bytes();
        let Some(next_payload_bytes) = self.payload_bytes.checked_add(incoming_payload_bytes)
        else {
            return self.reject_full(class, "payload byte accounting overflow");
        };
        if next_payload_bytes > CMD_QUEUE_MAX_PAYLOAD_BYTES {
            return self.reject_full(class, "payload byte cap");
        }
        if class == TmuxCommandClass::CoalescibleIntent {
            if let Some(lease) = conditional_commit.as_ref() {
                let target = lease.intent.target();
                if let Some(deferred) = self.retry_deferred_intents.get_mut(&target) {
                    let old_payload_bytes = deferred.command.mailbox_payload_bytes();
                    if !deferred.command.try_merge_newer(cmd.as_ref()) {
                        // A conditional target is a complete latest-wins
                        // identity. If a future intent type cannot merge in
                        // place, replacing the dormant command is still safe
                        // and prevents a stale generation from hiding behind a
                        // different dormant target.
                        deferred.command = cmd;
                    }
                    deferred.conditional_commit = Some(lease.clone());
                    let merged_payload_bytes = deferred.command.mailbox_payload_bytes();
                    self.payload_bytes = self
                        .payload_bytes
                        .checked_sub(old_payload_bytes)
                        .and_then(|retained| retained.checked_add(merged_payload_bytes))
                        .expect("tmux deferred-intent payload accounting invariant");
                    debug_assert!(self.payload_bytes <= CMD_QUEUE_MAX_PAYLOAD_BYTES);
                    self.publish_conditional_commit_lease(Some(lease.clone()));
                    self.merged_commands = self.merged_commands.saturating_add(1);
                    metrics::counter!(
                        "mux.tmux.command_mailbox.admitted",
                        "class" => class.label(),
                        "disposition" => "merged_deferred",
                    )
                    .increment(1);
                    return Ok(());
                }
            }
        }
        let can_merge_conditional = conditional_commit.as_ref().is_none_or(|lease| {
            self.queued_conditional_commits
                .get(&lease.intent.target())
                .is_some_and(|leases| !leases.is_empty())
        });
        let merged = can_merge_conditional && {
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
            if let Some(lease) = conditional_commit {
                let replaced = self.replace_last_queued_conditional_commit(lease.clone());
                debug_assert!(
                    replaced,
                    "merged tmux conditional command lost its queued generation"
                );
                self.publish_conditional_commit_lease(Some(lease));
            }
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
        if let Some(lease) = conditional_commit {
            self.register_queued_conditional_commit(lease.clone());
            self.publish_conditional_commit_lease(Some(lease));
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

    /// Admit a response-matched split transaction on its dedicated priority
    /// lane. This lane remains writable after ordinary producers are frozen,
    /// but never after final queue closure.
    fn push_split_transaction(
        &mut self,
        command: Box<dyn TmuxCommand>,
    ) -> Result<(), TmuxEnqueueError> {
        if self.closed {
            return Err(TmuxEnqueueError::Closed);
        }
        if !command.is_split_transaction() {
            return Err(TmuxEnqueueError::ClassMismatch);
        }
        let Some(failure_authority) = command.split_failure_authority() else {
            return Err(TmuxEnqueueError::ClassMismatch);
        };
        if matches!(
            failure_authority,
            TmuxSplitFailureAuthority::Compensation(_)
        ) {
            return Err(TmuxEnqueueError::ClassMismatch);
        }
        if self.terminal_barrier
            && matches!(
                failure_authority,
                TmuxSplitFailureAuthority::Baseline { .. }
                    | TmuxSplitFailureAuthority::Pending { .. }
            )
        {
            return Err(TmuxEnqueueError::Closed);
        }
        let class = command.mailbox_class();
        if !matches!(
            class,
            TmuxCommandClass::RequiredControl | TmuxCommandClass::TerminalControl
        ) {
            return Err(TmuxEnqueueError::ClassMismatch);
        }
        self.split_transaction_entries
            .try_reserve(1)
            .map_err(|_| TmuxEnqueueError::Full)?;
        self.split_transaction_entries.push_back(command);
        self.retained_by_class[class.index()] =
            self.retained_by_class[class.index()].saturating_add(1);
        self.split_cleanup_barrier = true;
        if let Some(preparing) = self.preparing.as_mut() {
            if !preparing.split_transaction {
                preparing.superseded = true;
            }
        }
        metrics::counter!(
            "mux.tmux.command_mailbox.admitted",
            "class" => class.label(),
            "disposition" => "split_transaction",
        )
        .increment(1);
        Ok(())
    }

    /// Occupy the cleanup lane before `split-window` can mutate tmux.  The
    /// boxed command and its immutable byte storage therefore need no
    /// allocation when rollback later claims this exact request.
    fn reserve_split_cleanup(
        &mut self,
        command: Box<dyn TmuxCommand>,
    ) -> Result<(), TmuxEnqueueError> {
        if self.closed || self.terminal_barrier {
            return Err(TmuxEnqueueError::Closed);
        }
        if command.mailbox_class() != TmuxCommandClass::TerminalControl
            || !matches!(
                command.split_failure_authority(),
                Some(TmuxSplitFailureAuthority::Compensation(_))
            )
        {
            return Err(TmuxEnqueueError::ClassMismatch);
        }
        if !self.can_admit_count(TmuxCommandClass::TerminalControl, 1) {
            return self.reject_full(
                TmuxCommandClass::TerminalControl,
                "preallocated split cleanup slot",
            );
        }
        self.split_cleanup_entries
            .try_reserve(1)
            .map_err(|_| TmuxEnqueueError::Full)?;
        self.split_cleanup_entries.push_back(TmuxSplitCleanupSlot {
            command,
            ready: false,
        });
        self.retained_by_class[TmuxCommandClass::TerminalControl.index()] =
            self.retained_by_class[TmuxCommandClass::TerminalControl.index()].saturating_add(1);
        self.split_cleanup_barrier = true;
        Ok(())
    }

    fn admit_prepared_split(
        &mut self,
        cleanup: Box<dyn TmuxCommand>,
        baseline: Box<dyn TmuxCommand>,
    ) -> Result<(), TmuxEnqueueError> {
        if self.closed || self.terminal_barrier {
            return Err(TmuxEnqueueError::Closed);
        }
        if baseline.mailbox_class() != TmuxCommandClass::RequiredControl
            || !matches!(
                baseline.split_failure_authority(),
                Some(TmuxSplitFailureAuthority::Baseline { .. })
            )
        {
            return Err(TmuxEnqueueError::ClassMismatch);
        }
        if self
            .len()
            .checked_add(2)
            .is_none_or(|next| next > CMD_QUEUE_MAX_DEPTH)
            || !self.can_admit_count(TmuxCommandClass::RequiredControl, 1)
            || !self.can_admit_count(TmuxCommandClass::TerminalControl, 1)
        {
            return Err(TmuxEnqueueError::Full);
        }
        self.split_cleanup_entries
            .try_reserve(1)
            .map_err(|_| TmuxEnqueueError::Full)?;
        self.split_transaction_entries
            .try_reserve(1)
            .map_err(|_| TmuxEnqueueError::Full)?;
        self.reserve_split_cleanup(cleanup)?;
        self.push_split_transaction(baseline)
    }

    fn arm_split_cleanup(
        &mut self,
        obligation: &Arc<TmuxSplitCleanupObligation>,
    ) -> Result<(), TmuxEnqueueError> {
        if self.closed {
            return Err(TmuxEnqueueError::Closed);
        }
        let Some(slot) = self.split_cleanup_entries.iter_mut().find(|slot| {
            matches!(
                slot.command().split_failure_authority(),
                Some(TmuxSplitFailureAuthority::Compensation(current))
                    if Arc::ptr_eq(&current, obligation)
            )
        }) else {
            return Err(TmuxEnqueueError::ClassMismatch);
        };
        if slot.ready {
            return Err(TmuxEnqueueError::ClassMismatch);
        }
        slot.ready = true;
        self.split_cleanup_barrier = true;
        if let Some(preparing) = self.preparing.as_mut() {
            if !preparing.split_transaction {
                preparing.superseded = true;
            }
        }
        Ok(())
    }

    fn has_ready_split_cleanup(&self) -> bool {
        self.split_cleanup_entries
            .iter()
            .any(TmuxSplitCleanupSlot::is_ready)
    }

    fn release_split_cleanup_barrier(&mut self) {
        if self.split_cleanup_entries.is_empty()
            && self.split_transaction_entries.is_empty()
            && self
                .in_flight
                .as_ref()
                .is_none_or(|in_flight| !in_flight.command.is_split_transaction())
            && self
                .preparing
                .as_ref()
                .is_none_or(|preparing| !preparing.split_transaction)
        {
            self.split_cleanup_barrier = false;
        }
    }

    fn freeze_for_split_cleanup(&mut self) {
        if !self.closed {
            self.split_cleanup_barrier = true;
            if let Some(preparing) = self.preparing.as_mut() {
                if !preparing.split_transaction {
                    preparing.superseded = true;
                }
            }
        }
    }

    fn has_split_transaction_work(&self) -> bool {
        self.has_ready_split_cleanup()
            || !self.split_transaction_entries.is_empty()
            || self
                .in_flight
                .as_ref()
                .is_some_and(|in_flight| in_flight.command.is_split_transaction())
            || self
                .preparing
                .as_ref()
                .is_some_and(|preparing| preparing.split_transaction)
    }

    fn remove_queued_split_compensation(
        &mut self,
        obligation: &TmuxSplitCleanupObligation,
    ) -> bool {
        let Some(index) = self.split_cleanup_entries.iter().position(|slot| {
            matches!(
                slot.command().split_failure_authority(),
                Some(TmuxSplitFailureAuthority::Compensation(current))
                    if std::ptr::eq(current.as_ref(), obligation)
            )
        }) else {
            return false;
        };
        let Some(slot) = self.split_cleanup_entries.remove(index) else {
            return false;
        };
        let command = slot.into_command();
        let class = command.mailbox_class();
        self.retained_by_class[class.index()] =
            self.retained_by_class[class.index()].saturating_sub(1);
        drop(command);
        true
    }

    fn remove_queued_split_reconciliation(&mut self, request_id: u64) -> bool {
        let Some(index) = self.split_transaction_entries.iter().position(|command| {
            matches!(
                command.split_failure_authority(),
                Some(TmuxSplitFailureAuthority::Reconciliation {
                    request_id: current,
                    ..
                }) if current == request_id
            )
        }) else {
            return false;
        };
        let Some(command) = self.split_transaction_entries.remove(index) else {
            return false;
        };
        let class = command.mailbox_class();
        self.retained_by_class[class.index()] =
            self.retained_by_class[class.index()].saturating_sub(1);
        drop(command);
        true
    }

    fn remove_queued_pending_split(&mut self, request_id: u64) -> bool {
        let Some(index) = self.split_transaction_entries.iter().position(|command| {
            matches!(
                command.split_failure_authority(),
                Some(
                    TmuxSplitFailureAuthority::Baseline {
                        request_id: current,
                        ..
                    } | TmuxSplitFailureAuthority::Pending {
                        request_id: current,
                        ..
                    }
                ) if current == request_id
            )
        }) else {
            return false;
        };
        let Some(command) = self.split_transaction_entries.remove(index) else {
            return false;
        };
        let class = command.mailbox_class();
        self.retained_by_class[class.index()] =
            self.retained_by_class[class.index()].saturating_sub(1);
        drop(command);
        true
    }

    fn split_failure_authority_for_sender(&self) -> Option<TmuxSplitFailureAuthority> {
        self.in_flight
            .as_ref()
            .and_then(|in_flight| in_flight.command.split_failure_authority())
            .or_else(|| {
                self.preparing
                    .as_ref()
                    .and_then(|preparing| preparing.split_failure_authority.clone())
            })
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
        if self.closed || self.terminal_barrier || self.split_cleanup_barrier {
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
            return self.reject_full(TmuxCommandClass::TerminalControl, "detach payload byte cap");
        }
        if !self.can_admit_count(TmuxCommandClass::TerminalControl, 1) {
            return self.reject_full(
                TmuxCommandClass::TerminalControl,
                "detach command count cap",
            );
        }
        self.durable_entries.push_front(command);
        self.retained_by_class[TmuxCommandClass::TerminalControl.index()] =
            self.retained_by_class[TmuxCommandClass::TerminalControl.index()].saturating_add(1);
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
        if self.closed || self.terminal_barrier || self.split_cleanup_barrier {
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

        let mut conditional_commits = Vec::with_capacity(commands.len());
        for command in &commands {
            conditional_commits.push(self.allocate_conditional_commit_lease(
                command.conditional_commit_intent(),
                TmuxCommandClass::RequiredControl,
            )?);
        }
        let next_depth = self.len().saturating_add(commands.len());
        let crossed_warning_threshold =
            self.len() <= CMD_QUEUE_WARNING_DEPTH && next_depth > CMD_QUEUE_WARNING_DEPTH;
        let incoming_count = commands.len();
        self.durable_entries.extend(commands);
        for lease in conditional_commits.into_iter().flatten() {
            self.register_queued_conditional_commit(lease.clone());
            self.publish_conditional_commit_lease(Some(lease));
        }
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

    pub(crate) fn close(&mut self) -> TmuxCmdQueueTeardown {
        self.closed = true;
        self.retained_by_class = [0; TmuxCommandClass::COUNT];
        self.durable_since_intent = 0;
        self.intents_since_retry_deferred_durable = 0;
        self.fresh_intents_since_retry_deferred_intent = 0;
        self.retry_deferred_intent_head = None;
        self.retry_deferred_intent_tail = None;
        self.payload_bytes = 0;
        TmuxCmdQueueTeardown {
            split_cleanup_entries: std::mem::take(&mut self.split_cleanup_entries),
            split_transaction_entries: std::mem::take(&mut self.split_transaction_entries),
            durable_entries: std::mem::take(&mut self.durable_entries),
            intent_entries: std::mem::take(&mut self.intent_entries),
            retry_deferred_durable: std::mem::take(&mut self.retry_deferred_durable),
            ready_retry_deferred_intents: std::mem::take(&mut self.ready_retry_deferred_intents),
            retry_deferred_intents: std::mem::take(&mut self.retry_deferred_intents),
            in_flight: self.in_flight.take(),
            preparing: self.preparing.take(),
            latest_conditional_commits: std::mem::take(&mut self.latest_conditional_commits),
            queued_conditional_commits: std::mem::take(&mut self.queued_conditional_commits),
            uncertain_remote_pane_sizes: std::mem::take(&mut self.uncertain_remote_pane_sizes),
        }
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
        self.split_cleanup_entries.is_empty()
            && self.split_transaction_entries.is_empty()
            && self.durable_entries.is_empty()
            && self.intent_entries.is_empty()
            && self.retry_deferred_durable.is_empty()
            && self.retry_deferred_intent_head.is_none()
            && self.retry_deferred_intent_tail.is_none()
            && self.retry_deferred_intents.is_empty()
            && self.ready_retry_deferred_intents.is_empty()
            && self.in_flight.is_none()
            && self.preparing.is_none()
    }

    #[cfg(test)]
    pub(crate) fn front(&self) -> Option<&dyn TmuxCommand> {
        self.in_flight
            .as_ref()
            .map(|in_flight| in_flight.command.as_ref())
            .or_else(|| {
                self.split_cleanup_entries
                    .iter()
                    .find(|slot| slot.is_ready())
                    .map(TmuxSplitCleanupSlot::command)
            })
            .or_else(|| self.split_transaction_entries.front().map(Box::as_ref))
            .or_else(|| {
                if self.should_service_intent() {
                    self.intent_entries.front().map(Box::as_ref)
                } else {
                    self.durable_entries.front().map(Box::as_ref)
                }
            })
            .or_else(|| {
                self.retry_deferred_durable
                    .front()
                    .map(|deferred| deferred.command.as_ref())
            })
            .or_else(|| {
                self.retry_deferred_intent_head
                    .and_then(|target| self.retry_deferred_intents.get(&target))
                    .map(|deferred| deferred.command.as_ref())
            })
    }

    #[cfg(test)]
    fn take_next_for_preparation(&mut self) -> Option<Box<dyn TmuxCommand>> {
        self.take_next_for_preparation_with_policy(true)
    }

    fn take_next_for_preparation_with_policy(
        &mut self,
        allow_durable: bool,
    ) -> Option<Box<dyn TmuxCommand>> {
        debug_assert!(self.preparing.is_none());
        if let Some(index) = self
            .split_cleanup_entries
            .iter()
            .position(TmuxSplitCleanupSlot::is_ready)
        {
            let command = self
                .split_cleanup_entries
                .remove(index)
                .expect("selected tmux split cleanup slot disappeared")
                .into_command();
            self.preparing = Some(PreparingTmuxCommand {
                class: command.mailbox_class(),
                payload_bytes: command.mailbox_payload_bytes(),
                conditional_commit: None,
                superseded: false,
                split_transaction: true,
                split_failure_authority: command.split_failure_authority(),
            });
            metrics::counter!(
                "mux.tmux.command_mailbox.serviced",
                "class" => command.mailbox_class().label(),
                "source" => "split_cleanup",
            )
            .increment(1);
            return Some(command);
        }
        if let Some(command) = self.split_transaction_entries.pop_front() {
            self.preparing = Some(PreparingTmuxCommand {
                class: command.mailbox_class(),
                payload_bytes: command.mailbox_payload_bytes(),
                conditional_commit: None,
                superseded: false,
                split_transaction: true,
                split_failure_authority: command.split_failure_authority(),
            });
            metrics::counter!(
                "mux.tmux.command_mailbox.serviced",
                "class" => command.mailbox_class().label(),
                "source" => "split_transaction",
            )
            .increment(1);
            return Some(command);
        }
        let service_intent = !allow_durable || self.should_service_intent();
        let command = if service_intent {
            let retry_intent_ready = self.retry_deferred_intent_is_ready();
            self.durable_since_intent = 0;
            let command = self.intent_entries.pop_front()?;
            if retry_intent_ready {
                self.fresh_intents_since_retry_deferred_intent = self
                    .fresh_intents_since_retry_deferred_intent
                    .saturating_add(1);
            } else {
                self.fresh_intents_since_retry_deferred_intent = 0;
            }
            if !self.retry_deferred_durable.is_empty() {
                self.intents_since_retry_deferred_durable =
                    self.intents_since_retry_deferred_durable.saturating_add(1);
            }
            command
        } else {
            self.durable_since_intent = self.durable_since_intent.saturating_add(1);
            self.durable_entries.pop_front()?
        };
        if command.awaits_clean_exit() {
            debug_assert!(self.terminal_barrier);
            debug_assert!(!self.terminal_command_dispatched);
            self.terminal_command_dispatched = true;
        }
        let conditional_intent = command.conditional_commit_intent();
        let conditional_commit = conditional_intent
            .as_ref()
            .and_then(|intent| self.take_queued_conditional_commit(intent.target()));
        let superseded = match (&conditional_intent, &conditional_commit) {
            (Some(intent), Some(lease)) => {
                lease.intent != *intent || !self.conditional_commit_is_current(lease)
            }
            (Some(_), None) => true,
            (None, _) => false,
        };
        self.preparing = Some(PreparingTmuxCommand {
            class: command.mailbox_class(),
            payload_bytes: command.mailbox_payload_bytes(),
            conditional_commit,
            superseded,
            split_transaction: command.is_split_transaction(),
            split_failure_authority: command.split_failure_authority(),
        });
        metrics::counter!(
            "mux.tmux.command_mailbox.serviced",
            "class" => command.mailbox_class().label(),
        )
        .increment(1);
        Some(command)
    }

    fn prepared_conditional_commit(&self) -> Option<TmuxConditionalCommitLease> {
        self.preparing
            .as_ref()
            .and_then(|preparing| preparing.conditional_commit.clone())
    }

    fn prepared_is_superseded(&self) -> bool {
        self.preparing
            .as_ref()
            .is_some_and(|preparing| preparing.superseded)
    }

    /// Revalidates exact conditional authority at the prepare/install
    /// linearization point. Preparation deliberately runs without the mailbox
    /// lock; a newer same-target admission during that interval must prevent
    /// the older bytes from ever reaching tmux.
    fn prepared_install_authority_is_current(
        &self,
        generation: u64,
        conditional_commit: Option<&TmuxConditionalCommit>,
    ) -> bool {
        let Some(preparing) = self.preparing.as_ref() else {
            return false;
        };
        if preparing.superseded {
            return false;
        }
        match (preparing.conditional_commit.as_ref(), conditional_commit) {
            (None, None) => true,
            (Some(lease), Some(commit)) => {
                commit.io_generation() == generation
                    && commit.lease() == lease
                    && self.conditional_commit_is_current(lease)
            }
            _ => false,
        }
    }

    fn restore_prepared_for_retry(
        &mut self,
        command: Box<dyn TmuxCommand>,
        prerequisite: TmuxPreparationPrerequisite,
    ) -> bool {
        let Some(preparing) = self.preparing.as_ref() else {
            return false;
        };
        if preparing.class != command.mailbox_class()
            || preparing.payload_bytes != command.mailbox_payload_bytes()
            || (preparing.class == TmuxCommandClass::RequiredControl
                && !self.retry_deferred_durable.is_empty())
        {
            return false;
        }
        let retry_intent_target = if preparing.class == TmuxCommandClass::CoalescibleIntent {
            let Some(lease) = preparing.conditional_commit.as_ref() else {
                // Retryable intents must have a stable coalescing identity;
                // otherwise one dormant target could retain unbounded newer
                // generations behind it.
                return false;
            };
            let target = lease.intent.target();
            if self.retry_deferred_intents.contains_key(&target) {
                return false;
            }
            Some(target)
        } else {
            None
        };
        let preparing = self
            .preparing
            .take()
            .expect("validated tmux preparation reservation disappeared");
        let deferred = DeferredTmuxCommand {
            command,
            conditional_commit: preparing.conditional_commit,
            retry_prerequisite: prerequisite,
            retry_ready: false,
            retry_intent_previous: None,
            retry_intent_next: None,
        };
        match preparing.class {
            TmuxCommandClass::CoalescibleIntent => {
                let target = retry_intent_target
                    .expect("validated retryable tmux intent lost its target identity");
                self.append_retry_deferred_intent(target, deferred);
            }
            TmuxCommandClass::RequiredControl => {
                // A blocked required command remains the durable FIFO head.
                // Intents may still bypass it, but later durable input/control
                // cannot overtake it. There can therefore be at most one such
                // deferred durable reservation.
                debug_assert!(self.retry_deferred_durable.is_empty());
                self.intents_since_retry_deferred_durable = 0;
                self.retry_deferred_durable.push_back(deferred);
            }
            TmuxCommandClass::LosslessInput | TmuxCommandClass::TerminalControl => return false,
        }
        true
    }

    fn retry_deferred_is_ready(&self, deferred: &DeferredTmuxCommand) -> bool {
        deferred
            .conditional_commit
            .as_ref()
            .is_some_and(|lease| !self.conditional_commit_is_current(lease))
            || deferred.retry_ready
    }

    fn should_force_retry_deferred_durable(&self) -> bool {
        self.intents_since_retry_deferred_durable >= CMD_QUEUE_DURABLE_SERVICE_QUANTUM
            && self
                .retry_deferred_durable
                .front()
                .is_some_and(|deferred| self.retry_deferred_is_ready(deferred))
    }

    fn retry_deferred_intent_is_ready(&self) -> bool {
        // Same-target admission updates the target-indexed deferred command
        // and its exact lease in place, so an intent cannot become stale while
        // dormant. Explicit prerequisite publication is therefore the sole
        // ready edge and remains O(1) on every producer scheduling check.
        !self.ready_retry_deferred_intents.is_empty()
    }

    fn should_force_retry_deferred_intent(&self) -> bool {
        self.retry_deferred_intent_is_ready()
            && (self.intent_entries.is_empty()
                || self.fresh_intents_since_retry_deferred_intent
                    >= CMD_QUEUE_RETRY_INTENT_SERVICE_QUANTUM)
            && (self.durable_entries.is_empty()
                || !self.retry_deferred_durable.is_empty()
                || self.durable_since_intent >= CMD_QUEUE_DURABLE_SERVICE_QUANTUM)
    }

    /// Apply the production mailbox policy for one preparation attempt.
    ///
    /// A parked required-control item remains the durable FIFO head. Intents
    /// may bypass it while its prerequisite is unchanged, but once it is ready
    /// no more than one bounded intent quantum may run before it is retried.
    fn take_next_for_sender_preparation(
        &mut self,
    ) -> Option<Result<Box<dyn TmuxCommand>, Box<dyn TmuxCommand>>> {
        debug_assert!(self.preparing.is_none());

        if self.has_ready_split_cleanup() || !self.split_transaction_entries.is_empty() {
            return self.take_next_for_preparation_with_policy(true).map(Ok);
        }

        if self.split_cleanup_barrier {
            return None;
        }

        if !self.terminal_barrier && self.should_force_retry_deferred_durable() {
            return self.take_retry_deferred_for_preparation();
        }

        if !self.terminal_barrier && self.should_force_retry_deferred_intent() {
            return self.take_retry_deferred_intent_for_preparation();
        }

        let allow_durable = self.terminal_barrier || self.retry_deferred_durable.is_empty();
        if let Some(command) = self.take_next_for_preparation_with_policy(allow_durable) {
            return Some(Ok(command));
        }
        if self.terminal_barrier {
            None
        } else {
            self.take_retry_deferred_for_preparation()
        }
    }

    /// Select at most one deferred command. A deferred durable entry is a FIFO
    /// barrier for later durable work but not for latency-sensitive intents.
    /// Stale conditional work is returned for destruction outside the mailbox
    /// lock and still counts against the caller's preparation quantum.
    fn take_retry_deferred_for_preparation(
        &mut self,
    ) -> Option<Result<Box<dyn TmuxCommand>, Box<dyn TmuxCommand>>> {
        let intent_ready = self.retry_deferred_intent_is_ready();
        let durable_ready = self
            .retry_deferred_durable
            .front()
            .is_some_and(|deferred| self.retry_deferred_is_ready(deferred));
        let take_durable = durable_ready
            && (!intent_ready
                || self.intents_since_retry_deferred_durable >= CMD_QUEUE_DURABLE_SERVICE_QUANTUM);
        if !intent_ready && !take_durable {
            return None;
        }
        if !take_durable {
            return self.take_retry_deferred_intent_for_preparation();
        }
        let deferred = {
            self.intents_since_retry_deferred_durable = 0;
            self.retry_deferred_durable.pop_front()?
        };
        self.begin_retry_preparation(deferred)
    }

    fn take_retry_deferred_intent_for_preparation(
        &mut self,
    ) -> Option<Result<Box<dyn TmuxCommand>, Box<dyn TmuxCommand>>> {
        let target = self.ready_retry_deferred_intents.pop_front()?;
        if !self.retry_deferred_durable.is_empty() {
            self.intents_since_retry_deferred_durable =
                self.intents_since_retry_deferred_durable.saturating_add(1);
        }
        self.fresh_intents_since_retry_deferred_intent = 0;
        self.durable_since_intent = 0;
        let deferred = self
            .remove_retry_deferred_intent(target)
            .expect("tmux ready retry FIFO lost its target-indexed entry");
        debug_assert!(deferred.retry_ready);
        self.begin_retry_preparation(deferred)
    }

    fn begin_retry_preparation(
        &mut self,
        deferred: DeferredTmuxCommand,
    ) -> Option<Result<Box<dyn TmuxCommand>, Box<dyn TmuxCommand>>> {
        let stale = deferred
            .conditional_commit
            .as_ref()
            .is_some_and(|lease| !self.conditional_commit_is_current(lease));
        if stale {
            self.release_deferred_accounting(deferred.command.as_ref());
            return Some(Err(deferred.command));
        }

        let command = deferred.command;
        self.preparing = Some(PreparingTmuxCommand {
            class: command.mailbox_class(),
            payload_bytes: command.mailbox_payload_bytes(),
            conditional_commit: deferred.conditional_commit,
            superseded: false,
            split_transaction: command.is_split_transaction(),
            split_failure_authority: command.split_failure_authority(),
        });
        metrics::counter!(
            "mux.tmux.command_mailbox.serviced",
            "class" => command.mailbox_class().label(),
            "source" => "retry",
        )
        .increment(1);
        Some(Ok(command))
    }

    fn release_deferred_accounting(&mut self, command: &dyn TmuxCommand) {
        let class = command.mailbox_class();
        self.payload_bytes = self
            .payload_bytes
            .saturating_sub(command.mailbox_payload_bytes());
        self.retained_by_class[class.index()] =
            self.retained_by_class[class.index()].saturating_sub(1);
    }

    fn release_prepared(&mut self) {
        if let Some(preparing) = self.preparing.take() {
            self.payload_bytes = self.payload_bytes.saturating_sub(preparing.payload_bytes);
            self.retained_by_class[preparing.class.index()] =
                self.retained_by_class[preparing.class.index()].saturating_sub(1);
        }
    }

    fn install_in_flight(
        &mut self,
        cmd: Box<dyn TmuxCommand>,
        generation: u64,
        conditional_commit: Option<TmuxConditionalCommit>,
    ) -> bool {
        if self.closed || self.in_flight.is_some() {
            self.release_prepared();
            false
        } else {
            debug_assert_eq!(
                self.preparing
                    .as_ref()
                    .map(|preparing| (preparing.class, preparing.payload_bytes)),
                Some((cmd.mailbox_class(), cmd.mailbox_payload_bytes()))
            );
            debug_assert_eq!(
                conditional_commit
                    .as_ref()
                    .map(TmuxConditionalCommit::lease),
                self.preparing
                    .as_ref()
                    .and_then(|preparing| preparing.conditional_commit.as_ref())
            );
            debug_assert!(
                conditional_commit
                    .as_ref()
                    .is_none_or(|commit| commit.io_generation() == generation)
            );
            if let Some(TmuxConditionalCommit::PaneSize { lease, .. }) = conditional_commit.as_ref()
            {
                if let TmuxConditionalCommitIntent::PaneSize { pane_id, .. } = &lease.intent {
                    // From this linearization point onward the command may
                    // reach tmux. Cached dimensions cannot suppress another
                    // resize until this exact current lease succeeds and its
                    // cache update commits.
                    self.uncertain_remote_pane_sizes.insert(*pane_id);
                }
            }
            self.preparing = None;
            let remaining_responses = cmd.expected_responses();
            debug_assert!(remaining_responses > 0);
            self.in_flight = Some(InFlightTmuxCommand {
                command: cmd,
                generation,
                remaining_responses,
                first_error: None,
                conditional_commit,
            });
            true
        }
    }

    fn record_in_flight_response(
        &mut self,
        response: &Guarded,
    ) -> Option<(
        Box<dyn TmuxCommand>,
        Guarded,
        u64,
        Option<TmuxConditionalCommit>,
    )> {
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
        Some((
            in_flight.command,
            response,
            in_flight.generation,
            in_flight.conditional_commit,
        ))
    }

    fn has_pending(&self) -> bool {
        if self.has_ready_split_cleanup() || !self.split_transaction_entries.is_empty() {
            return true;
        }
        if self.split_cleanup_barrier {
            return false;
        }
        if self.terminal_barrier && self.terminal_command_dispatched {
            return false;
        }
        if self.terminal_barrier || !self.intent_entries.is_empty() {
            return true;
        }
        if self.retry_deferred_intent_is_ready() {
            return true;
        }
        if let Some(deferred) = self.retry_deferred_durable.front() {
            return self.retry_deferred_is_ready(deferred);
        }
        !self.durable_entries.is_empty()
    }

    fn has_domain_detach_pending(&self) -> bool {
        self.terminal_barrier
    }

    fn should_service_intent(&self) -> bool {
        if self.split_cleanup_barrier {
            return false;
        }
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
    command: Option<Vec<u8>>,
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
                &self.command.as_ref().map_or(0, |command| command.len()),
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
    Response {
        generation: u64,
    },
    Terminal {
        clean_exit: bool,
    },
}

#[derive(Debug)]
struct TmuxIoWriteJob {
    generation: u64,
    kind: TmuxIoOperationKind,
    command: Option<Vec<u8>>,
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
    fn new(domain_id: DomainId, owner: Weak<TmuxDomainState>) -> std::io::Result<Self> {
        let (control_tx, control_rx) = bounded(TMUX_IO_CONTROL_CAPACITY);
        let thread_name = format!("tmux-io-supervisor-{domain_id}");
        let failure_owner = owner.clone();
        let spawn_result = std::thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                let outcome = catch_recoverable(
                    RecoverablePanicSite::MuxTmuxCallback,
                    std::panic::AssertUnwindSafe(|| {
                        run_tmux_io_supervisor(owner, control_rx);
                    }),
                );
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
        let outcome = catch_recoverable(
            RecoverablePanicSite::MuxTmuxCallback,
            std::panic::AssertUnwindSafe(|| execute_tmux_io_write(&owner, &job)),
        )
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
            message: format!("tmux {} I/O job had no command payload", job.kind.label()),
        };
    };

    let result: anyhow::Result<()> = match job.kind {
        TmuxIoOperationKind::Command => pane
            .writer()
            .write_all(command.as_ref())
            .map_err(anyhow::Error::from),
        TmuxIoOperationKind::Detach => {
            if let Some(local_pane) = pane.downcast_ref::<LocalPane>() {
                let command = match std::str::from_utf8(command.as_ref()) {
                    Ok(command) => command,
                    Err(err) => {
                        return TmuxIoWriteOutcome::Io {
                            error_kind: std::io::ErrorKind::InvalidData,
                            message: format!("tmux detach command was not UTF-8: {err}"),
                        };
                    }
                };
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
                    .write_all(command.as_ref())
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

fn run_tmux_io_supervisor(owner: Weak<TmuxDomainState>, control: Receiver<TmuxIoControl>) {
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
            let Some(remaining) =
                remaining_deadline(initial_guard_started.get(), initial_guard_budget.get())
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
                    domain.fail_tmux_io_operation(start.kind, start.generation, "start_timeout");
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
                    domain.fail_tmux_io_operation(start.kind, start.generation, "start_timeout");
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
                    domain.fail_tmux_io_operation(start.kind, start.generation, "start_timeout");
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
                    domain.fail_tmux_io_operation(start.kind, start.generation, "write_timeout");
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
                        }) if response_generation == generation && !guarded_response_received => {
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
                            metrics::histogram!("mux.tmux.io.clean_exit_seconds",)
                                .record(response_started.elapsed().as_secs_f64());
                            return;
                        }
                        Ok(TmuxIoControl::Terminal { .. })
                            if kind == TmuxIoOperationKind::Command =>
                        {
                            if domain.io_operation_is_current(kind, generation) {
                                domain.fail_tmux_io_operation(
                                    kind,
                                    generation,
                                    "terminal_before_response",
                                );
                            }
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
                            domain.fail_tmux_io_operation(kind, generation, "overlapping_start");
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
    output_lane: OnceLock<TmuxPaneOutputLane>,
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
    pub(crate) pane_registry: Mutex<()>,
    pub(crate) retired_panes: Mutex<HashSet<TmuxPaneId>>,
    pub(crate) remote_split_reservations: Mutex<HashMap<TmuxPaneId, Arc<TmuxRemoteSplitStateCell>>>,
    split_cleanup_obligations: Mutex<HashMap<u64, Arc<TmuxSplitCleanupObligation>>>,
    split_cleanup_quarantine: Mutex<VecDeque<TmuxSplitQuarantine>>,
    remote_split_identity_permits: AtomicUsize,
    pub tmux_session: Mutex<Option<TmuxSessionId>>,
    pub support_commands: Mutex<HashMap<String, String>>,
    pub attach_state: Mutex<AttachState>,
    pub(crate) notification_subscription_gate: Mutex<()>,
    pub notification_sub_id: Mutex<Option<usize>>,
    config_reload_sub: Mutex<Option<config::ConfigSubscription>>,
    backlog_limits_dirty: AtomicBool,
    pending_splits: Mutex<HashMap<u64, PendingTmuxSplit>>,
    next_split_request_id: AtomicU64,
    pub backlog: Mutex<TmuxBacklog>,
    #[cfg(test)]
    test_conditional_commits: AtomicUsize,
    #[cfg(test)]
    test_command_preparations: AtomicUsize,
    #[cfg(test)]
    test_send_runnables_scheduled: AtomicUsize,
    #[cfg(test)]
    pub(crate) test_split_config_panic: AtomicU8,
    #[cfg(test)]
    pub(crate) test_split_output_failure: AtomicU8,
    #[cfg(test)]
    pub(crate) test_retire_split_domain_before_local_commit: AtomicBool,
    #[cfg(test)]
    test_io_deadlines: Mutex<Option<TmuxIoDeadlines>>,
}

pub struct TmuxDomain {
    pub(crate) inner: Arc<TmuxDomainState>,
}

#[derive(Debug, Default)]
struct TmuxLifecycle {
    terminal: bool,
    terminalizing: bool,
    terminalizing_clean_exit: bool,
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
            self.owner
                .fail_sender_operation("sender_runnable_cancelled");
        }
    }
}

struct ResponseBarrierLease {
    owner: Arc<TmuxDomainState>,
    abandoned_split_result: Option<(TmuxSplitFailureAuthority, Guarded, u64)>,
    completed: bool,
}

impl ResponseBarrierLease {
    fn recover(&mut self, reason: &'static str) {
        if let Some((authority, response, generation)) = self.abandoned_split_result.take() {
            self.owner
                .recover_abandoned_split_result(authority, &response, generation, reason);
        } else {
            self.owner.transition_to_exit_and_schedule_detach();
        }
        self.completed = true;
    }
}

impl Drop for ResponseBarrierLease {
    fn drop(&mut self) {
        if !self.completed {
            log::error!(
                "tmux domain {} lost its scheduled command-result task; detaching instead of \
                 stranding the protocol barrier",
                self.owner.domain_id
            );
            self.recover("command_result_task_cancelled");
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

    fn fail_pane_output_gap(&self, pane_id: TmuxPaneId, gap: TmuxPaneOutputGap) {
        metrics::counter!(
            "mux.tmux.output.gap",
            "reason" => gap.label(),
        )
        .increment(1);
        log::error!(
            "tmux pane {pane_id} output in domain {} lost bounded stream authority \
             ({}); detaching instead of replaying a partial terminal stream",
            self.domain_id,
            gap.label()
        );
        self.transition_to_exit_and_schedule_detach();
    }

    fn enqueue_materialized_pane_output(
        &self,
        pane_id: TmuxPaneId,
        remote_pane: &RefTmuxRemotePane,
        payload: Vec<u8>,
    ) -> Result<(), TmuxPaneOutputGap> {
        let limits = TmuxPaneOutputLimits::current();
        let schedule = {
            let mut pane = remote_pane.lock();
            if pane.output_state == TmuxPaneOutputState::Retired {
                return Ok(());
            }
            if matches!(
                pane.output_state,
                TmuxPaneOutputState::AwaitingCapture | TmuxPaneOutputState::Captured
            ) {
                pane.output_ingress.capture_raced = true;
            }
            pane.output_ingress.push_back(payload, limits)?;
            metrics::histogram!("mux.tmux.output.pane_queue_bytes")
                .record(pane.output_ingress.queued_bytes as f64);
            metrics::histogram!("mux.tmux.output.pane_queue_items")
                .record(pane.output_ingress.chunks.len() as f64);
            if pane.output_state == TmuxPaneOutputState::Ready
                && !pane.output_ingress.drain_scheduled
                && !pane.output_ingress.chunks.is_empty()
            {
                pane.output_ingress.drain_scheduled = true;
                true
            } else {
                false
            }
        };

        if !schedule {
            return Ok(());
        }
        let result = self
            .output_lane
            .get()
            .ok_or(TmuxPaneOutputGap::DrainLaneClosed)?
            .schedule(pane_id);
        if let Err(gap) = result {
            remote_pane.lock().output_ingress.drain_scheduled = false;
            return Err(gap);
        }
        metrics::counter!("mux.tmux.output.drain_scheduled").increment(1);
        Ok(())
    }

    pub(crate) fn schedule_ready_pane_output(
        &self,
        pane_id: TmuxPaneId,
        remote_pane: &RefTmuxRemotePane,
    ) -> anyhow::Result<()> {
        self.enqueue_materialized_pane_output(pane_id, remote_pane, Vec::new())
            .map_err(|gap| {
                anyhow::anyhow!(
                    "tmux pane {pane_id} output drain admission failed: {}",
                    gap.label()
                )
            })
    }

    fn enqueue_tmux_output(
        &self,
        pane_id: TmuxPaneId,
        payload: Vec<u8>,
    ) -> Result<(), TmuxPaneOutputGap> {
        let remote_pane = {
            let pane_map = self.remote_panes.lock();
            pane_map.get(&pane_id).cloned()
        };
        if let Some(remote_pane) = remote_pane {
            return self.enqueue_materialized_pane_output(pane_id, &remote_pane, payload);
        }

        // Serialize only the rare absent-pane decision with publication and
        // retirement. The global pane map itself remains lookup-only and is
        // never held while backlog storage or a pane gate is acquired.
        let _registry = self.pane_registry.lock();
        let remote_pane = {
            let pane_map = self.remote_panes.lock();
            pane_map.get(&pane_id).cloned()
        };
        if let Some(remote_pane) = remote_pane {
            drop(_registry);
            return self.enqueue_materialized_pane_output(pane_id, &remote_pane, payload);
        }
        if self.retired_panes.lock().contains(&pane_id) {
            log::debug!("discarding late output for retired tmux pane {pane_id}");
            return Ok(());
        }
        if let Some(reservation) = self.remote_split_reservations.lock().get(&pane_id) {
            match reservation
                .load()
                .map_err(|_| TmuxPaneOutputGap::InvalidState)?
            {
                TmuxRemoteSplitState::Reserved => {}
                TmuxRemoteSplitState::Retired => {
                    log::debug!("discarding late output for rolled-back tmux split pane {pane_id}");
                    return Ok(());
                }
                TmuxRemoteSplitState::Published => {
                    return Err(TmuxPaneOutputGap::InvalidState);
                }
            }
        }

        let _ = self.backlog_limits_dirty.swap(false, Ordering::AcqRel);
        let limits = TmuxBacklogLimits::current();
        let recovery_required = {
            let mut backlog = self.backlog.lock();
            backlog.append_owned_with_limits(pane_id, payload, limits);
            backlog.requires_recovery()
        };
        if recovery_required {
            return Err(TmuxPaneOutputGap::BacklogRecoveryRequired);
        }
        log::debug!("tmux pane {pane_id} has not been attached");
        Ok(())
    }

    fn drain_pane_output_quantum(&self, pane_id: TmuxPaneId) -> TmuxPaneDrainDisposition {
        const MAX_WRITES_PER_QUANTUM: usize = 16;

        // Terminal cleanup drains the authoritative pane map and releases
        // retained ingress storage. Enroll the complete lookup/write quantum
        // in the lifecycle fence so cleanup cannot race between the terminal
        // check and the nonblocking socket write.
        let Some(_active_operation) = self.begin_active_operation() else {
            return TmuxPaneDrainDisposition::Complete;
        };
        let limits = TmuxPaneOutputLimits::current();
        if limits.write_quantum_bytes == 0 {
            return TmuxPaneDrainDisposition::Gap(TmuxPaneOutputGap::InvalidQuantum);
        }
        let remote_pane = {
            let pane_map = self.remote_panes.lock();
            pane_map.get(&pane_id).cloned()
        };
        let Some(remote_pane) = remote_pane else {
            return TmuxPaneDrainDisposition::Complete;
        };
        let mut pane = remote_pane.lock();
        if !pane.output_ingress.drain_scheduled {
            return TmuxPaneDrainDisposition::Complete;
        }
        if pane.output_state == TmuxPaneOutputState::Retired {
            pane.output_ingress.clear();
            pane.output_ingress.drain_scheduled = false;
            return TmuxPaneDrainDisposition::Complete;
        }
        if pane.output_state != TmuxPaneOutputState::Ready {
            pane.output_ingress.drain_scheduled = false;
            return TmuxPaneDrainDisposition::Gap(TmuxPaneOutputGap::InvalidState);
        }

        let TmuxRemotePane {
            output_write,
            output_ingress,
            ..
        } = &mut *pane;
        let mut drained = 0_usize;
        let mut writes = 0_usize;
        while drained < limits.write_quantum_bytes && writes < MAX_WRITES_PER_QUANTUM {
            let Some(front) = output_ingress.chunks.front() else {
                output_ingress.front_offset = 0;
                output_ingress.drain_scheduled = false;
                metrics::histogram!("mux.tmux.output.pane_queue_bytes").record(0.0);
                metrics::histogram!("mux.tmux.output.pane_queue_items").record(0.0);
                return TmuxPaneDrainDisposition::Complete;
            };
            let remaining_quantum = limits.write_quantum_bytes - drained;
            let end = front.len().min(
                output_ingress
                    .front_offset
                    .saturating_add(remaining_quantum),
            );
            let write_result = output_write.write(&front[output_ingress.front_offset..end]);
            writes += 1;
            match write_result {
                Ok(0) => {
                    return TmuxPaneDrainDisposition::Failed(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "tmux pane output socket accepted zero bytes",
                    ));
                }
                Ok(written) => {
                    output_ingress.front_offset += written;
                    output_ingress.queued_bytes =
                        output_ingress.queued_bytes.saturating_sub(written);
                    drained += written;
                    let front_complete = output_ingress
                        .chunks
                        .front()
                        .is_some_and(|front| output_ingress.front_offset == front.len());
                    if front_complete {
                        output_ingress.chunks.pop_front();
                        output_ingress.front_offset = 0;
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    metrics::counter!("mux.tmux.output.would_block").increment(1);
                    return TmuxPaneDrainDisposition::Blocked;
                }
                Err(err) => return TmuxPaneDrainDisposition::Failed(err),
            }
        }

        if drained > 0 {
            metrics::counter!("mux.tmux.output.drained_bytes")
                .increment(u64::try_from(drained).unwrap_or(u64::MAX));
        }
        metrics::histogram!("mux.tmux.output.pane_queue_bytes")
            .record(output_ingress.queued_bytes as f64);
        metrics::histogram!("mux.tmux.output.pane_queue_items")
            .record(output_ingress.chunks.len() as f64);
        if output_ingress.chunks.is_empty() {
            output_ingress.front_offset = 0;
            output_ingress.drain_scheduled = false;
            TmuxPaneDrainDisposition::Complete
        } else {
            TmuxPaneDrainDisposition::Ready
        }
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
        let (terminalizing, first_transition, authoritative_clean_exit, obligations) = {
            let mut lifecycle = self.lifecycle.lock();
            if lifecycle.terminal {
                (false, false, lifecycle.clean_exit, Vec::new())
            } else {
                let has_pending_splits = !self.pending_splits.lock().is_empty();
                let obligations: Vec<_> = self
                    .split_cleanup_obligations
                    .lock()
                    .values()
                    .cloned()
                    .collect();
                let split_work = !obligations.is_empty()
                    || has_pending_splits
                    || self.cmd_queue.lock().has_split_transaction_work();
                if split_work {
                    let first_terminalizing = !lifecycle.terminalizing;
                    lifecycle.terminalizing = true;
                    lifecycle.terminalizing_clean_exit = false;
                    self.cmd_queue.lock().freeze_for_split_cleanup();
                    (
                        true,
                        first_terminalizing,
                        lifecycle.terminalizing_clean_exit,
                        obligations,
                    )
                } else {
                    let mut state = self.state.lock();
                    lifecycle.terminalizing = false;
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
                    (false, true, requested_clean_exit, Vec::new())
                }
            }
        };

        if terminalizing {
            self.notification_intents.lock().close();
            self.unsubscribe_notification();
            for obligation in obligations {
                if requested_clean_exit {
                    if obligation.claim() {
                        obligation.finish_claimed(false, "tmux exited before split compensation");
                    }
                } else if let Err(error) = self.claim_remote_split_compensation(&obligation) {
                    log::error!(
                        "tmux domain {} could not start terminal split compensation for request {} pane {:?}: {error:#}",
                        self.domain_id,
                        obligation.request_id,
                        obligation.pane_id()
                    );
                }
            }
            if requested_clean_exit {
                let pending_request_ids: Vec<_> =
                    self.pending_splits.lock().keys().copied().collect();
                for request_id in pending_request_ids {
                    self.fail_split_reconciliation(
                        request_id,
                        "tmux exited before split identity was reconciled",
                        Vec::new(),
                    );
                }
                {
                    let mut lifecycle = self.lifecycle.lock();
                    lifecycle.terminalizing = false;
                    lifecycle.terminal = true;
                    lifecycle.clean_exit = true;
                    lifecycle.io_operation = None;
                    lifecycle.detach_disposition = TerminalDetachDisposition::NotNeeded;
                    *self.state.lock() = State::Exit;
                    self.clean_exit_requested.store(true, Ordering::Release);
                }
                self.publish_terminal_transition(true, true);
                return;
            }
            if first_transition {
                log::debug!(
                    "tmux domain {} entered bounded split-cleanup terminalization",
                    self.domain_id
                );
            }
            let _ = self.schedule_send_next_command();
            self.maybe_finish_terminalizing();
            return;
        }
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

    fn record_split_quarantine(
        &self,
        request_id: u64,
        candidates: Vec<TmuxPaneId>,
        reason: impl Into<String>,
    ) {
        let reason = reason.into();
        let mut quarantine = self.split_cleanup_quarantine.lock();
        if quarantine.len() == TMUX_SPLIT_QUARANTINE_LIMIT {
            let _ = quarantine.pop_front();
        }
        quarantine.push_back(TmuxSplitQuarantine {
            request_id,
            candidates,
            reason: reason.clone(),
        });
        metrics::counter!(
            "mux.tmux.split_cleanup.quarantined",
            "reason" => "ambiguous_or_failed",
        )
        .increment(1);
        log::error!(
            "tmux domain {} quarantined split request {}: {}",
            self.domain_id,
            request_id,
            reason
        );
    }

    fn finish_split_cleanup_obligation(
        &self,
        obligation: &TmuxSplitCleanupObligation,
        succeeded: bool,
        reason: &'static str,
    ) {
        {
            let _lifecycle = self.lifecycle.lock();
            let mut obligations = self.split_cleanup_obligations.lock();
            let exact = obligations
                .get(&obligation.request_id)
                .is_some_and(|current| std::ptr::eq(current.as_ref(), obligation));
            if exact {
                let _ = obligations.remove(&obligation.request_id);
            } else {
                log::error!(
                    "tmux split cleanup request {} lost its exact obligation map entry",
                    obligation.request_id
                );
            }
        }
        let _ = self
            .cmd_queue
            .lock()
            .remove_queued_split_compensation(obligation);
        if !succeeded {
            self.record_split_quarantine(
                obligation.request_id,
                obligation.pane_id().into_iter().collect(),
                reason,
            );
        }
        self.release_split_cleanup_barrier_if_idle();
        if !self.lifecycle.lock().terminalizing {
            let _ = self.schedule_send_next_command();
        }
        self.maybe_finish_terminalizing();
    }

    fn release_split_cleanup_barrier_if_idle(&self) {
        let lifecycle = self.lifecycle.lock();
        if lifecycle.terminalizing
            || !self.pending_splits.lock().is_empty()
            || !self.split_cleanup_obligations.lock().is_empty()
        {
            return;
        }
        self.cmd_queue.lock().release_split_cleanup_barrier();
    }

    fn maybe_finish_terminalizing(&self) {
        let transition = {
            let mut lifecycle = self.lifecycle.lock();
            if !lifecycle.terminalizing || lifecycle.terminal {
                return;
            }
            if !self.pending_splits.lock().is_empty()
                || !self.split_cleanup_obligations.lock().is_empty()
                || self.cmd_queue.lock().has_split_transaction_work()
                || lifecycle.io_operation.is_some()
                || *self.state.lock() != State::Idle
            {
                return;
            }
            let clean_exit = lifecycle.terminalizing_clean_exit;
            lifecycle.terminalizing = false;
            lifecycle.terminal = true;
            lifecycle.clean_exit = clean_exit;
            lifecycle.io_operation = None;
            lifecycle.detach_disposition = if clean_exit {
                TerminalDetachDisposition::NotNeeded
            } else {
                TerminalDetachDisposition::Pending
            };
            *self.state.lock() = State::Exit;
            if clean_exit {
                self.clean_exit_requested.store(true, Ordering::Release);
            }
            clean_exit
        };
        self.publish_terminal_transition(true, transition);
    }

    fn begin_active_operation(&self) -> Option<ActiveTmuxOperation<'_>> {
        let mut lifecycle = self.lifecycle.lock();
        if lifecycle.terminal || lifecycle.terminalizing {
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
        if lifecycle.terminal || lifecycle.terminalizing {
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

    fn begin_protocol_operation(&self) -> Option<ActiveTmuxOperation<'_>> {
        let mut lifecycle = self.lifecycle.lock();
        if lifecycle.terminal {
            return None;
        }
        let Some(next) = lifecycle.active_operations.checked_add(1) else {
            log::error!(
                "tmux domain {} active-operation counter overflow; rejecting protocol work",
                self.domain_id
            );
            return None;
        };
        lifecycle.active_operations = next;
        Some(ActiveTmuxOperation { owner: self })
    }

    fn begin_owned_protocol_operation(self: &Arc<Self>) -> Option<OwnedActiveTmuxOperation> {
        let mut lifecycle = self.lifecycle.lock();
        if lifecycle.terminal {
            return None;
        }
        let Some(next) = lifecycle.active_operations.checked_add(1) else {
            log::error!(
                "tmux domain {} active-operation counter overflow; rejecting owned protocol work",
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
        alloc_nonwrapping_atomic_u64(&self.next_io_generation).ok_or_else(|| {
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

    fn install_io_operation(&self, kind: TmuxIoOperationKind, generation: u64) -> bool {
        let mut lifecycle = self.lifecycle.lock();
        Self::install_io_operation_locked(&mut lifecycle, kind, generation)
    }

    fn install_io_operation_locked(
        lifecycle: &mut TmuxLifecycle,
        kind: TmuxIoOperationKind,
        generation: u64,
    ) -> bool {
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

    fn claim_io_response(&self, kind: TmuxIoOperationKind, generation: u64) -> bool {
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
        lifecycle.terminalizing = false;
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
        let split_failure = self
            .cmd_queue
            .lock()
            .in_flight
            .as_ref()
            .and_then(|in_flight| {
                if in_flight.generation == generation {
                    in_flight.command.split_failure_authority()
                } else {
                    None
                }
            });
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
        self.fail_split_transaction_authority(split_failure);
        self.invalidate_launcher_after_io_failure(reason);
        self.publish_terminal_transition(true, false);
    }

    fn fail_initial_guard(&self, reason: &'static str) {
        if !self.try_claim_failure_terminal(|_lifecycle, state| state == State::WaitForInitialGuard)
        {
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
        self.fail_io_supervisor_with_authority(reason, None);
    }

    fn fail_sender_operation(&self, reason: &'static str) {
        let split_failure = self.cmd_queue.lock().split_failure_authority_for_sender();
        self.fail_io_supervisor_with_authority(reason, split_failure);
    }

    fn fail_io_supervisor_with_authority(
        &self,
        reason: &'static str,
        explicit_split_failure: Option<TmuxSplitFailureAuthority>,
    ) {
        let split_failure = explicit_split_failure
            .or_else(|| self.cmd_queue.lock().split_failure_authority_for_sender());
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
        self.fail_split_transaction_authority(split_failure);
        self.invalidate_launcher_after_io_failure(reason);
        self.publish_terminal_transition(true, false);
    }

    fn fail_split_transaction_authority(&self, authority: Option<TmuxSplitFailureAuthority>) {
        match authority {
            Some(TmuxSplitFailureAuthority::Baseline { request_id, .. }) => {
                let _ = self.fail_pending_split(
                    request_id,
                    anyhow::anyhow!("split baseline I/O ended before an exact scoped response"),
                );
            }
            Some(TmuxSplitFailureAuthority::Compensation(obligation)) => {
                if obligation.status() == TmuxSplitCleanupStatus::Claimed {
                    obligation.finish_claimed(false, "tmux split compensation timed out");
                }
            }
            Some(TmuxSplitFailureAuthority::Pending { request_id, .. }) => {
                self.fail_split_reconciliation(
                    request_id,
                    "split command I/O ended before an exact response",
                    Vec::new(),
                );
            }
            Some(TmuxSplitFailureAuthority::Reconciliation { request_id, .. }) => {
                self.fail_split_reconciliation(
                    request_id,
                    "split reconciliation I/O ended before an exact response",
                    Vec::new(),
                );
            }
            None => {}
        }
    }

    fn recover_abandoned_split_result(
        self: &Arc<Self>,
        authority: TmuxSplitFailureAuthority,
        response: &Guarded,
        generation: u64,
        reason: &'static str,
    ) {
        match authority {
            TmuxSplitFailureAuthority::Baseline { request_id, .. } => {
                let _ = self.fail_pending_split(
                    request_id,
                    anyhow::anyhow!(
                        "tmux split request {request_id} lost its scoped baseline result callback"
                    ),
                );
            }
            TmuxSplitFailureAuthority::Pending {
                request_id,
                target_pane_id,
            } => {
                if response.error {
                    let _ = self.fail_pending_split(
                        request_id,
                        anyhow::anyhow!(
                            "tmux split request {request_id} lost its result callback after a guarded error"
                        ),
                    );
                } else {
                    match parse_split_pane_identity(&response.output) {
                        SplitPaneIdentityParse::Exact(pane_id)
                        | SplitPaneIdentityParse::RecoverableTrailingOutput { pane_id, .. } => {
                            match self.pending_split_identity_is_known(request_id, pane_id) {
                                Ok(false) => {
                                    match self.compensate_pending_split_identity(
                                        request_id,
                                        target_pane_id,
                                        pane_id,
                                        format!(
                                            "tmux split request {request_id} lost its result callback; exact pane %{pane_id} is being compensated"
                                        ),
                                    ) {
                                        Ok(true) => {}
                                        Ok(false) => self.record_split_quarantine(
                                            request_id,
                                            vec![pane_id],
                                            "abandoned split result lost its pending request",
                                        ),
                                        Err(error) => log::error!(
                                            "tmux split request {request_id} could not compensate abandoned result pane %{pane_id}: {error:#}"
                                        ),
                                    }
                                }
                                Ok(true) => self.fail_split_reconciliation(
                                    request_id,
                                    "abandoned split result collided with retained authority",
                                    vec![pane_id],
                                ),
                                Err(error) => {
                                    log::error!(
                                        "tmux split request {request_id} could not validate abandoned result pane %{pane_id}: {error:#}"
                                    );
                                    self.fail_split_reconciliation(
                                        request_id,
                                        "abandoned split result authority validation failed",
                                        vec![pane_id],
                                    );
                                }
                            }
                        }
                        SplitPaneIdentityParse::Unresolved(error) => {
                            log::error!(
                                "tmux split request {request_id} lost an unresolved result callback: {error}"
                            );
                            self.fail_split_reconciliation(
                                request_id,
                                "abandoned split result had no safe exact compensation identity",
                                Vec::new(),
                            );
                        }
                    }
                }
            }
            TmuxSplitFailureAuthority::Reconciliation {
                request_id,
                target_pane_id,
            } => {
                if response.error {
                    self.fail_split_reconciliation(
                        request_id,
                        "abandoned split reconciliation received a guarded error",
                        Vec::new(),
                    );
                } else if let Err(error) =
                    self.finish_split_reconciliation(request_id, target_pane_id, &response.output)
                {
                    log::error!(
                        "tmux split request {request_id} could not recover its abandoned reconciliation result: {error:#}"
                    );
                    self.fail_split_reconciliation(
                        request_id,
                        "abandoned split reconciliation could not establish exact authority",
                        Vec::new(),
                    );
                }
            }
            TmuxSplitFailureAuthority::Compensation(obligation) => {
                if obligation.status() == TmuxSplitCleanupStatus::Claimed {
                    obligation.finish_claimed(
                        !response.error,
                        if response.error {
                            "abandoned split compensation returned an error"
                        } else {
                            "abandoned split compensation was acknowledged"
                        },
                    );
                }
            }
        }

        // The result task owned this exact response barrier. If it disappears,
        // discard only its fenced trailing events, restore the command state to
        // Idle, and let the dedicated split lane transmit any exact
        // compensation before final terminal teardown.
        let abandoned_protocol_events = {
            let _ingress = self.protocol_ingress.lock();
            let mut barrier = self.protocol_barrier.lock();
            let mut lifecycle = self.lifecycle.lock();
            if lifecycle.io_operation.is_some_and(|operation| {
                operation.kind == TmuxIoOperationKind::Command && operation.generation == generation
            }) {
                lifecycle.io_operation = None;
            }
            if !lifecycle.terminal {
                let mut state = self.state.lock();
                if matches!(
                    *state,
                    State::WaitingForResponse | State::ProcessingResponse
                ) {
                    *state = State::Idle;
                } else {
                    log::error!(
                        "tmux domain {} abandoned split result from unexpected protocol state {:?}",
                        self.domain_id,
                        *state
                    );
                }
            }
            drop(lifecycle);
            barrier.clear()
        };
        drop(abandoned_protocol_events);
        log::error!(
            "tmux domain {} recovered an abandoned split result at {reason}; terminalizing after exact cleanup",
            self.domain_id
        );
        self.request_terminal(false);
        if !self.is_terminal() {
            let _ = self.schedule_send_next_command();
            self.maybe_finish_terminalizing();
        }
    }

    fn abandon_command_result(
        self: &Arc<Self>,
        authority: Option<TmuxSplitFailureAuthority>,
        response: &Guarded,
        generation: u64,
        reason: &'static str,
    ) {
        let Some(authority) = authority else {
            self.transition_to_exit_and_schedule_detach();
            return;
        };
        self.recover_abandoned_split_result(authority, response, generation, reason);
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
            let mut remote_pane = remote_pane.lock();
            remote_pane.output_state = TmuxPaneOutputState::Retired;
            remote_pane.output_ingress.clear();
            remote_pane
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
        self.remote_split_reservations.lock().clear();
        let abandoned_split_cleanup: Vec<_> = self
            .split_cleanup_obligations
            .lock()
            .drain()
            .map(|(_, obligation)| obligation)
            .collect();
        for obligation in abandoned_split_cleanup {
            let prior = {
                let mut state = obligation.state.lock();
                let prior = state.status;
                state.status = TmuxSplitCleanupStatus::Failed;
                prior
            };
            self.record_split_quarantine(
                obligation.request_id,
                obligation.pane_id().into_iter().collect(),
                format!("terminal cleanup observed unresolved split obligation in state {prior:?}"),
            );
            obligation
                .child_state
                .mark_exited(portable_pty::ExitStatus::with_signal(
                    "tmux split cleanup abandoned during terminal teardown",
                ));
        }
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
        for (request_id, mut pending) in pending_splits {
            pending.promise.err(anyhow::anyhow!(
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
        domain: &DomainOperationGuard,
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
        let retirement_started = mux.domain_was_detached_if_guard(domain);
        let exact_instance_absent = retirement_started
            || mux
                .get_domain(expected_inner.domain_id)
                .is_none_or(|current| !current.same_registration(domain));

        let clean_exit = {
            let mut lifecycle = expected_inner.lifecycle.lock();
            if lifecycle.detach_disposition == TerminalDetachDisposition::Claimed {
                lifecycle.detach_disposition = TerminalDetachDisposition::Attempted;
            }
            lifecycle.finalization_in_progress = false;
            lifecycle.finalized = exact_instance_absent;
            lifecycle.clean_exit
        };
        if retirement_started && clean_exit {
            expected_inner
                .clean_detach_completed
                .store(true, Ordering::Release);
        }
    }

    fn finalize_launcher_tmux_binding(&self, mux: &Arc<Mux>, should_invalidate_launcher: bool) {
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
        if lifecycle.terminal || lifecycle.terminalizing {
            anyhow::bail!(
                "tmux domain {} became terminal or terminalizing before subscription publication",
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
        self.transition_state_with_lifecycle(&lifecycle, expected, next)
    }

    fn transition_state_with_lifecycle(
        &self,
        lifecycle: &TmuxLifecycle,
        expected: State,
        next: State,
    ) -> bool {
        debug_assert_ne!(next, State::Exit);
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

    /// Reserve both retained-identity budget and backing map capacity before
    /// `split-window` is admitted to tmux. Once the command is externally
    /// visible, every successful response can therefore install an exact
    /// cancellation-owned tombstone without allocation or cap failure.
    fn reserve_remote_split_identity(
        self: &Arc<Self>,
        request_id: u64,
        child_state: Arc<TmuxChildState>,
    ) -> anyhow::Result<(
        TmuxRetainedPaneIdentityPermit,
        Arc<TmuxSplitCleanupObligation>,
        Vec<TmuxPaneId>,
    )> {
        let cleanup = TmuxSplitCleanupObligation::new(self, request_id, child_state)?;
        let mut baseline_remote_pane_ids = Vec::new();
        baseline_remote_pane_ids
            .try_reserve_exact(TMUX_SPLIT_REMOTE_BASELINE_LIMIT)
            .map_err(|error| anyhow::anyhow!("reserve scoped tmux split baseline: {error}"))?;
        let _registry = self.pane_registry.lock();
        let retired_panes = self.retired_panes.lock();
        let mut reservations = self.remote_split_reservations.lock();
        let mut cleanup_obligations = self.split_cleanup_obligations.lock();
        let permits = self.remote_split_identity_permits.load(Ordering::Acquire);
        let retained_identities = retired_panes
            .len()
            .checked_add(reservations.len())
            .and_then(|retained| retained.checked_add(permits))
            .ok_or_else(|| anyhow::anyhow!("tmux retained pane identity count overflow"))?;
        anyhow::ensure!(
            retained_identities < RETIRED_PANE_TOMBSTONE_LIMIT,
            "tmux retained-pane identity cap {RETIRED_PANE_TOMBSTONE_LIMIT} exceeded before split command admission"
        );
        permits
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("tmux remote split identity permit overflow"))?;
        let reserved_suffix = permits
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("tmux remote split reservation suffix overflow"))?;
        reservations.try_reserve(reserved_suffix).map_err(|error| {
            anyhow::anyhow!("reserve tmux split identity before command admission: {error}")
        })?;
        cleanup_obligations
            .try_reserve(reserved_suffix)
            .map_err(|error| {
                anyhow::anyhow!(
                    "reserve tmux split cleanup authority before command admission: {error}"
                )
            })?;
        anyhow::ensure!(
            !cleanup_obligations.contains_key(&request_id),
            "duplicate tmux split cleanup request id {request_id}"
        );
        let observed_permits = self
            .remote_split_identity_permits
            .try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(1)
            })
            .map_err(|_| anyhow::anyhow!("tmux remote split identity permit overflow"))?;
        debug_assert!(observed_permits <= permits);
        let prior_cleanup = cleanup_obligations.insert(request_id, Arc::clone(&cleanup));
        debug_assert!(prior_cleanup.is_none());
        Ok((
            TmuxRetainedPaneIdentityPermit {
                owner: Arc::downgrade(self),
                armed: true,
            },
            cleanup,
            baseline_remote_pane_ids,
        ))
    }

    pub(crate) fn remote_split_identity_permit_count_locked(&self) -> usize {
        self.remote_split_identity_permits.load(Ordering::Acquire)
    }

    pub(crate) fn finish_split_baseline(
        self: &Arc<Self>,
        request_id: u64,
        command_target_remote_pane_id: TmuxPaneId,
        output: &str,
    ) -> anyhow::Result<()> {
        if self.lifecycle.lock().terminalizing {
            let _ = self.fail_pending_split(
                request_id,
                anyhow::anyhow!(
                    "tmux split request {request_id} terminalized before split-window admission"
                ),
            );
            return Ok(());
        }
        let command = {
            let mut pending_splits = self.pending_splits.lock();
            let pending = pending_splits
                .get_mut(&request_id)
                .with_context(|| format!("missing pending tmux split request {request_id}"))?;
            anyhow::ensure!(
                pending.target_remote_pane_id == command_target_remote_pane_id
                    && !pending.baseline_complete
                    && !pending.reconciling,
                "tmux split request {request_id} lost baseline phase authority"
            );
            pending.baseline_remote_pane_ids.clear();
            for line in output.lines().filter(|line| !line.trim().is_empty()) {
                let (session_id, window_id, pane_id, _token) = parse_scoped_split_pane_line(line)?;
                anyhow::ensure!(
                    session_id == pending.target_session_id
                        && window_id == pending.target_window_id,
                    "tmux split baseline escaped its exact session/window scope"
                );
                anyhow::ensure!(
                    pending.baseline_remote_pane_ids.len() < TMUX_SPLIT_REMOTE_BASELINE_LIMIT,
                    "tmux split scoped baseline exceeded {TMUX_SPLIT_REMOTE_BASELINE_LIMIT} panes"
                );
                pending.baseline_remote_pane_ids.push(pane_id);
            }
            pending.baseline_remote_pane_ids.sort_unstable();
            pending.baseline_remote_pane_ids.dedup();
            anyhow::ensure!(
                pending
                    .baseline_remote_pane_ids
                    .binary_search(&pending.target_remote_pane_id)
                    .is_ok(),
                "tmux split target pane was absent from its authoritative remote baseline"
            );
            pending.baseline_complete = true;
            pending
                .split_command
                .take()
                .context("tmux split request lost its preallocated split command")?
        };
        if let Err(error) = self.cmd_queue.lock().push_split_transaction(command) {
            let _ = self.fail_pending_split(
                request_id,
                anyhow::anyhow!(
                    "tmux split request {request_id} could not admit its prepared split command: {error}"
                ),
            );
            return Err(anyhow::anyhow!(
                "cannot admit prepared split for request {request_id}: {error}"
            ));
        }
        Ok(())
    }

    pub(crate) fn resolve_pending_split(
        self: &Arc<Self>,
        request_id: u64,
        command_target_remote_pane_id: TmuxPaneId,
        pane_id: TmuxPaneId,
    ) -> anyhow::Result<bool> {
        let Some((mut pending, reservation)) = self.take_pending_split_reservation(
            request_id,
            command_target_remote_pane_id,
            pane_id,
        )?
        else {
            return Ok(false);
        };

        if self.lifecycle.lock().terminalizing {
            let _ = pending.promise.err(anyhow::anyhow!(
                "tmux split request {request_id} completed while domain {} was terminalizing",
                self.domain_id
            ));
            drop(reservation);
            return Ok(true);
        }

        let accepted = pending.promise.ok(reservation);
        if !accepted {
            log::debug!(
                "tmux split request {request_id} completed after its local waiter was cancelled"
            );
        }
        Ok(true)
    }

    fn take_pending_split_reservation(
        self: &Arc<Self>,
        request_id: u64,
        command_target_remote_pane_id: TmuxPaneId,
        pane_id: TmuxPaneId,
    ) -> anyhow::Result<Option<(PendingTmuxSplit, TmuxRemoteSplitReservation)>> {
        let lifecycle = self.lifecycle.lock();
        anyhow::ensure!(
            !lifecycle.terminal,
            "tmux split request {request_id} completed after final terminal teardown"
        );
        let Some(mut pending) = self.pending_splits.lock().remove(&request_id) else {
            return Ok(None);
        };

        let reservation_result = (|| {
            anyhow::ensure!(
                pending.target_remote_pane_id == command_target_remote_pane_id,
                "tmux split request {request_id} target changed from {} to {command_target_remote_pane_id}",
                pending.target_remote_pane_id
            );
            anyhow::ensure!(
                pending.baseline_complete && !pending.reconciling,
                "tmux split request {request_id} produced an identity outside its split phase"
            );
            let _registry = self.pane_registry.lock();
            let identity_permit = pending
                .identity_permit
                .take()
                .context("tmux split response lost its retained-identity permit")?;
            identity_permit.validate_locked(self)?;
            anyhow::ensure!(
                !self.retired_panes.lock().contains(&pane_id),
                "tmux split returned retired remote pane id {pane_id}"
            );
            anyhow::ensure!(
                !self.remote_panes.lock().contains_key(&pane_id),
                "tmux split returned already-materialized remote pane id {pane_id}"
            );
            anyhow::ensure!(
                self.mirror_index
                    .lock()
                    .checked_local_pane_for_remote(pane_id)?
                    .is_none(),
                "tmux split returned already-indexed remote pane id {pane_id}"
            );
            let mut reservations = self.remote_split_reservations.lock();
            anyhow::ensure!(
                !reservations.contains_key(&pane_id),
                "tmux split returned already-reserved remote pane id {pane_id}"
            );
            anyhow::ensure!(
                self.split_cleanup_obligations
                    .lock()
                    .get(&request_id)
                    .is_some_and(|cleanup| Arc::ptr_eq(cleanup, &pending.cleanup)),
                "tmux split request {request_id} lost its preallocated cleanup authority"
            );
            pending.cleanup.install_remote_identity(pane_id)?;
            let prior = reservations.insert(pane_id, Arc::clone(&pending.state));
            debug_assert!(prior.is_none());
            identity_permit.consume_locked(self);
            Ok(TmuxRemoteSplitReservation {
                owner: Arc::clone(self),
                request_id,
                target_remote_pane_id: pending.target_remote_pane_id,
                remote_pane_id: pane_id,
                child_state: Arc::clone(&pending.child_state),
                state: Arc::clone(&pending.state),
                cleanup: Arc::clone(&pending.cleanup),
                published_gate: None,
                published_local_pane_id: None,
                published_window_id: None,
                output_reservation: None,
                armed: true,
            })
        })();

        drop(lifecycle);
        match reservation_result {
            Ok(reservation) => Ok(Some((pending, reservation))),
            Err(error) => {
                let message = format!(
                    "tmux split request {request_id} could not reserve remote pane {pane_id}: {error:#}"
                );
                let _ = pending.promise.err(anyhow::anyhow!(message.clone()));
                self.record_split_quarantine(request_id, vec![pane_id], message.clone());
                self.transition_to_exit_and_schedule_detach();
                Err(anyhow::anyhow!(message))
            }
        }
    }

    pub(crate) fn compensate_pending_split_identity(
        self: &Arc<Self>,
        request_id: u64,
        command_target_remote_pane_id: TmuxPaneId,
        pane_id: TmuxPaneId,
        reason: String,
    ) -> anyhow::Result<bool> {
        let Some((mut pending, reservation)) = self.take_pending_split_reservation(
            request_id,
            command_target_remote_pane_id,
            pane_id,
        )?
        else {
            return Ok(false);
        };
        let _ = pending.promise.err(anyhow::anyhow!(reason));
        drop(reservation);
        Ok(true)
    }

    pub(crate) fn pending_split_identity_is_known(
        &self,
        request_id: u64,
        pane_id: TmuxPaneId,
    ) -> anyhow::Result<bool> {
        anyhow::ensure!(
            self.pending_splits.lock().contains_key(&request_id),
            "missing pending tmux split request {request_id}"
        );
        let _registry = self.pane_registry.lock();
        Ok(self.retired_panes.lock().contains(&pane_id)
            || self.remote_panes.lock().contains_key(&pane_id)
            || self
                .mirror_index
                .lock()
                .checked_local_pane_for_remote(pane_id)?
                .is_some()
            || self.remote_split_reservations.lock().contains_key(&pane_id))
    }

    pub(crate) fn begin_split_reconciliation(
        &self,
        request_id: u64,
        command_target_remote_pane_id: TmuxPaneId,
        diagnostic: String,
    ) -> anyhow::Result<()> {
        let admission = {
            let mut lifecycle = self.lifecycle.lock();
            anyhow::ensure!(
                !lifecycle.terminal,
                "tmux split request {request_id} cannot reconcile after terminal teardown"
            );
            let mut pending_splits = self.pending_splits.lock();
            let pending = pending_splits
                .get_mut(&request_id)
                .with_context(|| format!("missing pending tmux split request {request_id}"))?;
            anyhow::ensure!(
                pending.target_remote_pane_id == command_target_remote_pane_id,
                "tmux split request {request_id} target changed before reconciliation"
            );
            anyhow::ensure!(
                pending.baseline_complete && !pending.reconciling,
                "tmux split request {request_id} entered reconciliation twice"
            );
            pending.reconciling = true;
            let reconciliation = pending
                .reconcile_command
                .take()
                .context("tmux split request lost its preallocated reconciliation command")?;
            lifecycle.terminalizing = true;
            lifecycle.terminalizing_clean_exit = false;
            let mut queue = self.cmd_queue.lock();
            queue.freeze_for_split_cleanup();
            queue.push_split_transaction(reconciliation)
        };
        if let Err(error) = admission {
            self.fail_split_reconciliation(
                request_id,
                "split reconciliation command admission failed",
                Vec::new(),
            );
            return Err(anyhow::anyhow!(
                "cannot admit split reconciliation for request {request_id}: {error}"
            ));
        }
        self.notification_intents.lock().close();
        self.unsubscribe_notification();
        log::error!(
            "tmux domain {} entered split identity reconciliation for request {}: {}",
            self.domain_id,
            request_id,
            diagnostic
        );
        if let Err(error) = self.schedule_send_next_command() {
            let removed = self
                .cmd_queue
                .lock()
                .remove_queued_split_reconciliation(request_id);
            debug_assert!(removed);
            self.fail_split_reconciliation(
                request_id,
                "split reconciliation scheduling failed",
                Vec::new(),
            );
            return Err(anyhow::anyhow!(
                "cannot schedule split reconciliation for request {request_id}: {error}"
            ));
        }
        Ok(())
    }

    pub(crate) fn finish_split_reconciliation(
        self: &Arc<Self>,
        request_id: u64,
        command_target_remote_pane_id: TmuxPaneId,
        output: &str,
    ) -> anyhow::Result<()> {
        let (target_session_id, target_window_id, request_token, mut candidates) = {
            let mut pending_splits = self.pending_splits.lock();
            let pending = pending_splits
                .get_mut(&request_id)
                .with_context(|| format!("missing pending tmux split request {request_id}"))?;
            anyhow::ensure!(
                pending.reconciling
                    && pending.target_remote_pane_id == command_target_remote_pane_id,
                "tmux split request {request_id} lost exact reconciliation authority"
            );
            (
                pending.target_session_id,
                pending.target_window_id,
                std::mem::take(&mut pending.request_token),
                std::mem::take(&mut pending.baseline_remote_pane_ids),
            )
        };
        candidates.clear();
        let mut tagged_pane = None;
        let mut multiple_tagged_panes = false;
        for line in output.lines().filter(|line| !line.trim().is_empty()) {
            let (session_id, window_id, pane_id, token) = match parse_scoped_split_pane_line(line) {
                Ok(row) => row,
                Err(error) => {
                    self.fail_split_reconciliation(
                        request_id,
                        "scoped split reconciliation returned a malformed row",
                        candidates,
                    );
                    return Err(error);
                }
            };
            if session_id != target_session_id || window_id != target_window_id {
                self.fail_split_reconciliation(
                    request_id,
                    "split reconciliation escaped its exact session/window scope",
                    candidates,
                );
                anyhow::bail!("split reconciliation returned an out-of-scope row");
            }
            if candidates.len() == TMUX_SPLIT_REMOTE_BASELINE_LIMIT {
                self.fail_split_reconciliation(
                    request_id,
                    "scoped split reconciliation exceeded its preallocated pane bound",
                    candidates,
                );
                anyhow::bail!(
                    "split reconciliation exceeded {TMUX_SPLIT_REMOTE_BASELINE_LIMIT} panes"
                );
            }
            candidates.push(pane_id);
            if token == request_token {
                match tagged_pane {
                    None => tagged_pane = Some(pane_id),
                    Some(current) if current == pane_id => {}
                    Some(_) => multiple_tagged_panes = true,
                }
            }
        }
        candidates.sort_unstable();
        candidates.dedup();
        if multiple_tagged_panes || tagged_pane.is_none() {
            self.fail_split_reconciliation(
                request_id,
                if multiple_tagged_panes {
                    "split reconciliation found multiple panes carrying the request token"
                } else {
                    "split reconciliation found no pane carrying the request token"
                },
                candidates,
            );
            anyhow::bail!("split reconciliation lacked one exact request-token witness");
        }
        let pane_id = tagged_pane.expect("checked exact tmux split token witness");
        match self.pending_split_identity_is_known(request_id, pane_id) {
            Ok(false) => {}
            Ok(true) => {
                self.fail_split_reconciliation(
                    request_id,
                    "request-token reconciliation candidate collided with live local authority",
                    candidates,
                );
                anyhow::bail!(
                    "split reconciliation candidate %{pane_id} is not exclusively request-owned"
                );
            }
            Err(error) => {
                self.fail_split_reconciliation(
                    request_id,
                    "request-token reconciliation authority check failed",
                    candidates,
                );
                return Err(error);
            }
        }
        anyhow::ensure!(
            self.compensate_pending_split_identity(
                request_id,
                command_target_remote_pane_id,
                pane_id,
                format!(
                    "tmux split request {request_id} returned ambiguous output; exact token witness %{pane_id} is being compensated"
                ),
            )?,
            "missing pending tmux split request {request_id}"
        );
        Ok(())
    }

    pub(crate) fn fail_split_reconciliation(
        &self,
        request_id: u64,
        reason: &'static str,
        mut candidates: Vec<TmuxPaneId>,
    ) {
        candidates.sort_unstable();
        candidates.dedup();
        let pending = self.pending_splits.lock().remove(&request_id);
        let Some(mut pending) = pending else {
            return;
        };
        pending.cleanup.fail_without_remote_identity(reason);
        let _ = pending.promise.err(anyhow::anyhow!(
            "tmux split request {request_id} quarantined: {reason}"
        ));
        self.record_split_quarantine(request_id, candidates, reason);
        self.maybe_finish_terminalizing();
    }

    pub(crate) fn fail_pending_split(&self, request_id: u64, err: anyhow::Error) -> bool {
        let pending = self.pending_splits.lock().remove(&request_id);
        if let Some(mut pending) = pending {
            let _ = self
                .cmd_queue
                .lock()
                .remove_queued_pending_split(request_id);
            pending
                .cleanup
                .complete_without_remote_effect("tmux split cancelled before remote effect");
            pending.promise.err(err);
            self.release_split_cleanup_barrier_if_idle();
            self.maybe_finish_terminalizing();
            true
        } else {
            false
        }
    }

    #[cfg(test)]
    pub(crate) fn reserve_test_remote_split(
        self: &Arc<Self>,
        request_id: u64,
        target_remote_pane_id: TmuxPaneId,
        remote_pane_id: TmuxPaneId,
    ) -> anyhow::Result<TmuxRemoteSplitReservation> {
        let child_state = Arc::new(TmuxChildState::new());
        let (identity_permit, cleanup, _baseline_remote_pane_ids) =
            self.reserve_remote_split_identity(request_id, Arc::clone(&child_state))?;
        if let Err(error) =
            self.cmd_queue
                .lock()
                .reserve_split_cleanup(Box::new(CompensateSplitPane {
                    obligation: Arc::clone(&cleanup),
                }))
        {
            cleanup.complete_without_remote_effect(
                "test split cleanup admission failed before remote effect",
            );
            return Err(anyhow::anyhow!(
                "reserve test tmux split cleanup slot: {error}"
            ));
        }
        let state = Arc::new(TmuxRemoteSplitStateCell::new());
        let _registry = self.pane_registry.lock();
        identity_permit.validate_locked(self)?;
        anyhow::ensure!(
            !self.retired_panes.lock().contains(&remote_pane_id)
                && !self.remote_panes.lock().contains_key(&remote_pane_id)
                && self
                    .mirror_index
                    .lock()
                    .checked_local_pane_for_remote(remote_pane_id)?
                    .is_none(),
            "test remote split identity {remote_pane_id} already exists"
        );
        let mut reservations = self.remote_split_reservations.lock();
        anyhow::ensure!(
            !reservations.contains_key(&remote_pane_id),
            "test remote split identity {remote_pane_id} is already reserved"
        );
        anyhow::ensure!(
            self.split_cleanup_obligations
                .lock()
                .get(&request_id)
                .is_some_and(|current| Arc::ptr_eq(current, &cleanup)),
            "test remote split request {request_id} lost cleanup authority"
        );
        cleanup.install_remote_identity(remote_pane_id)?;
        let prior_reservation = reservations.insert(remote_pane_id, Arc::clone(&state));
        debug_assert!(prior_reservation.is_none());
        identity_permit.consume_locked(self);
        Ok(TmuxRemoteSplitReservation {
            owner: Arc::clone(self),
            request_id,
            target_remote_pane_id,
            remote_pane_id,
            child_state,
            state,
            cleanup,
            published_gate: None,
            published_local_pane_id: None,
            published_window_id: None,
            output_reservation: None,
            armed: true,
        })
    }

    fn claim_remote_split_compensation(
        &self,
        obligation: &Arc<TmuxSplitCleanupObligation>,
    ) -> anyhow::Result<bool> {
        if !obligation.claim() {
            return Ok(false);
        }
        let enqueue = self.cmd_queue.lock().arm_split_cleanup(obligation);
        if let Err(error) = enqueue {
            obligation.finish_claimed(false, "tmux split compensation admission failed");
            return Err(anyhow::anyhow!(
                "tmux split compensation admission failed for request {} pane {:?}: {error}",
                obligation.request_id,
                obligation.pane_id()
            ));
        }
        if let Err(error) = self.schedule_send_next_command() {
            let removed = self
                .cmd_queue
                .lock()
                .remove_queued_split_compensation(obligation);
            debug_assert!(removed);
            obligation.finish_claimed(false, "tmux split compensation scheduling failed");
            return Err(anyhow::anyhow!(
                "tmux split compensation scheduling failed for request {} pane {:?}: {error}",
                obligation.request_id,
                obligation.pane_id()
            ));
        }
        Ok(true)
    }

    /// Transfer a synchronous published-pane removal onto the exact cleanup
    /// slot reserved before `split-window`.  `TmuxChildKiller` calls this before
    /// constructing an ordinary `KillPane`, so queue saturation and allocator
    /// failure cannot strand the remote child at the PaneAdded callback cut.
    pub(crate) fn claim_published_split_cleanup(
        &self,
        pane_id: TmuxPaneId,
        child_state: &Arc<TmuxChildState>,
    ) -> anyhow::Result<bool> {
        let obligation = {
            let obligations = self.split_cleanup_obligations.lock();
            obligations.values().find_map(|obligation| {
                obligation
                    .claim_published_child(pane_id, child_state)
                    .then(|| Arc::clone(obligation))
            })
        };
        let Some(obligation) = obligation else {
            return Ok(false);
        };
        if let Err(error) = self.cmd_queue.lock().arm_split_cleanup(&obligation) {
            obligation.finish_claimed(false, "published split cleanup transfer failed");
            return Err(anyhow::anyhow!(
                "published split cleanup transfer failed for pane %{pane_id}: {error}"
            ));
        }
        if let Err(error) = self.schedule_send_next_command() {
            let removed = self
                .cmd_queue
                .lock()
                .remove_queued_split_compensation(&obligation);
            debug_assert!(removed);
            obligation.finish_claimed(false, "published split cleanup scheduling failed");
            return Err(anyhow::anyhow!(
                "published split cleanup scheduling failed for pane %{pane_id}: {error}"
            ));
        }
        Ok(true)
    }

    fn rollback_remote_split(&self, reservation: &TmuxRemoteSplitReservation) {
        let mut inconsistent = false;
        {
            let _registry = self.pane_registry.lock();
            let reservations = self.remote_split_reservations.lock();
            let exact_reservation = reservations
                .get(&reservation.remote_pane_id)
                .is_some_and(|state| Arc::ptr_eq(state, &reservation.state));
            if !exact_reservation {
                if !self.is_terminal() {
                    log::error!(
                        "tmux split request {} lost exact remote reservation for pane {}",
                        reservation.request_id,
                        reservation.remote_pane_id
                    );
                    inconsistent = true;
                }
            } else {
                match reservation.state.load() {
                    Ok(TmuxRemoteSplitState::Reserved) => {
                        if reservation
                            .state
                            .transition(
                                TmuxRemoteSplitState::Reserved,
                                TmuxRemoteSplitState::Retired,
                            )
                            .is_err()
                        {
                            inconsistent = true;
                        }
                    }
                    Ok(TmuxRemoteSplitState::Published) => {
                        let mut remote_panes = self.remote_panes.lock();
                        let mut mirror_index = self.mirror_index.lock();
                        let mut gui_tabs = self.gui_tabs.lock();
                        let removed = remote_panes.remove(&reservation.remote_pane_id);
                        if !matches!(
                            (&removed, &reservation.published_gate),
                            (Some(removed), Some(expected)) if Arc::ptr_eq(removed, expected)
                        ) {
                            inconsistent = true;
                        }
                        match mirror_index.unregister_pane(reservation.remote_pane_id) {
                            Ok(local_pane_id)
                                if local_pane_id == reservation.published_local_pane_id => {}
                            _ => inconsistent = true,
                        }
                        match reservation.published_window_id {
                            Some(window_id) => {
                                let removed_membership =
                                    gui_tabs.get_mut(&window_id).is_some_and(|tab| {
                                        tab.panes.remove(&reservation.remote_pane_id)
                                    });
                                if !removed_membership {
                                    inconsistent = true;
                                }
                            }
                            None => inconsistent = true,
                        }
                        if reservation
                            .state
                            .transition(
                                TmuxRemoteSplitState::Published,
                                TmuxRemoteSplitState::Retired,
                            )
                            .is_err()
                        {
                            inconsistent = true;
                        }
                        if let Some(remote_gate) = removed {
                            let mut remote = remote_gate.lock();
                            remote.output_state = TmuxPaneOutputState::Retired;
                            remote.output_ingress.clear();
                        }
                    }
                    Ok(TmuxRemoteSplitState::Retired) => {}
                    Err(error) => {
                        log::error!("cannot retire tmux split reservation: {error:#}");
                        inconsistent = true;
                    }
                }
            }
            drop(reservations);
            self.backlog
                .lock()
                .remove_many(&[reservation.remote_pane_id]);
        }

        let compensation_failed =
            if let Err(error) = self.claim_remote_split_compensation(&reservation.cleanup) {
                log::error!(
                    "tmux split request {} could not compensate remote pane {}: {error:#}",
                    reservation.request_id,
                    reservation.remote_pane_id
                );
                true
            } else {
                false
            };
        if inconsistent || compensation_failed {
            self.transition_to_exit_and_schedule_detach();
        }
    }

    fn publish_reserved_remote_split(
        &self,
        reservation: &TmuxRemoteSplitReservation,
    ) -> anyhow::Result<()> {
        let remote_gate = reservation
            .published_gate
            .as_ref()
            .context("tmux split publication lacks its prepared remote gate")?;
        let local_pane_id = reservation
            .published_local_pane_id
            .context("tmux split publication lacks its local pane identity")?;
        let remote_window_id = reservation
            .published_window_id
            .context("tmux split publication lacks its remote window identity")?;

        {
            let remote = remote_gate.lock();
            anyhow::ensure!(
                remote.local_pane_id == local_pane_id
                    && remote.pane_id == reservation.remote_pane_id
                    && remote.window_id == remote_window_id
                    && Arc::ptr_eq(&remote.child_state, &reservation.child_state)
                    && remote.output_state == TmuxPaneOutputState::Fresh,
                "prepared tmux split pane identities changed before mirror publication"
            );
        }

        let _registry = self.pane_registry.lock();
        anyhow::ensure!(
            !self.is_terminal(),
            "tmux domain {} detached before split mirror publication",
            self.domain_id
        );
        anyhow::ensure!(
            !self
                .retired_panes
                .lock()
                .contains(&reservation.remote_pane_id),
            "reserved tmux split pane {} was retired before publication",
            reservation.remote_pane_id
        );
        let reservations = self.remote_split_reservations.lock();
        anyhow::ensure!(
            reservations
                .get(&reservation.remote_pane_id)
                .is_some_and(|state| Arc::ptr_eq(state, &reservation.state)),
            "tmux split pane {} lost its exact reservation before publication",
            reservation.remote_pane_id
        );
        anyhow::ensure!(
            reservation.state.load()? == TmuxRemoteSplitState::Reserved,
            "tmux split pane {} is not in reserved state before publication",
            reservation.remote_pane_id
        );

        let mut remote_panes = self.remote_panes.lock();
        anyhow::ensure!(
            !remote_panes.contains_key(&reservation.remote_pane_id),
            "tmux split pane {} became materialized before publication",
            reservation.remote_pane_id
        );
        remote_panes
            .try_reserve(1)
            .map_err(|error| anyhow::anyhow!("reserve tmux remote-pane mirror: {error}"))?;

        let mut mirror_index = self.mirror_index.lock();
        mirror_index.prepare_pane_registration(local_pane_id, reservation.remote_pane_id)?;
        let mut gui_tabs = self.gui_tabs.lock();
        let gui_tab = gui_tabs.get_mut(&remote_window_id).ok_or_else(|| {
            anyhow::anyhow!("tmux split window {remote_window_id} is no longer attached")
        })?;
        anyhow::ensure!(
            !gui_tab.panes.contains(&reservation.remote_pane_id),
            "tmux split pane {} is already attached to window {remote_window_id}",
            reservation.remote_pane_id
        );
        gui_tab
            .panes
            .try_reserve(1)
            .map_err(|error| anyhow::anyhow!("reserve tmux window pane membership: {error}"))?;

        let mut remote = remote_gate.lock();
        let limits = TmuxBacklogLimits::current();
        let backlog_drain = {
            let mut backlog = self.backlog.lock();
            backlog.refresh_limits(limits);
            anyhow::ensure!(
                !backlog.requires_global_resync(),
                "tmux split pane {} cannot recover from a global output gap",
                reservation.remote_pane_id
            );
            backlog.take(reservation.remote_pane_id)
        };
        match backlog_drain {
            Some(TmuxBacklogDrain::ResyncRequired) => anyhow::bail!(
                "tmux split pane {} initial output is gapped",
                reservation.remote_pane_id
            ),
            Some(TmuxBacklogDrain::Bytes(chunks)) => {
                remote
                    .output_ingress
                    .prepend(chunks, TmuxPaneOutputLimits::current())
                    .map_err(|gap| {
                        anyhow::anyhow!(
                            "tmux split pane {} initial output exceeded its live queue: {gap:?}",
                            reservation.remote_pane_id
                        )
                    })?;
            }
            None => {}
        }

        let prior = remote_panes.insert(reservation.remote_pane_id, Arc::clone(remote_gate));
        debug_assert!(prior.is_none());
        mirror_index.commit_pane_registration(local_pane_id, reservation.remote_pane_id);
        let inserted = gui_tab.panes.insert(reservation.remote_pane_id);
        debug_assert!(inserted);
        reservation
            .state
            .transition(
                TmuxRemoteSplitState::Reserved,
                TmuxRemoteSplitState::Published,
            )
            .expect(
                "exact reserved split state remains stable under the pane-registry transaction",
            );
        drop(remote);
        drop(gui_tabs);
        drop(mirror_index);
        drop(remote_panes);
        drop(reservations);
        drop(_registry);

        Ok(())
    }

    fn alloc_split_request_id(&self) -> anyhow::Result<u64> {
        alloc_nonwrapping_atomic_u64(&self.next_split_request_id)
            .ok_or_else(|| anyhow::anyhow!("tmux split request id space exhausted"))
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

    pub fn advance(self: &Arc<Self>, events: Box<Vec<Event>>) {
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
        if let Some((command, response, generation, conditional_commit)) = completed_response {
            self.schedule_command_result(command, response, generation, conditional_commit);
        }
    }

    fn process_protocol_events(
        self: &Arc<Self>,
        events: Vec<Event>,
    ) -> Option<(
        Box<dyn TmuxCommand>,
        Guarded,
        u64,
        Option<TmuxConditionalCommit>,
    )> {
        let mut events = events.into_iter();
        while let Some(event) = events.next() {
            if matches!(&event, Event::Exit { .. }) {
                self.transition_to_clean_exit();
                return None;
            }

            if self.lifecycle.lock().terminalizing && !matches!(&event, Event::Guarded(_)) {
                metrics::counter!(
                    "mux.tmux.protocol_event.skipped",
                    "reason" => "split_cleanup_terminalizing",
                )
                .increment(1);
                continue;
            }

            let _active_operation = self.begin_protocol_operation()?;
            let state = *self.state.lock();
            log::debug!("tmux: {:?} in state {:?}", event, state);
            let event = match event {
                Event::Output { pane, text } => {
                    if let Err(gap) = self.enqueue_tmux_output(pane, text) {
                        self.fail_pane_output_gap(pane, gap);
                        return None;
                    }
                    continue;
                }
                event => event,
            };
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
                        if let Some((cmd, resp, generation, conditional_commit)) =
                            cmd_queue.record_in_flight_response(response)
                        {
                            let split_failure_authority = cmd.split_failure_authority();
                            let io_kind = if cmd.awaits_clean_exit() {
                                TmuxIoOperationKind::Detach
                            } else {
                                TmuxIoOperationKind::Command
                            };
                            drop(cmd_queue);
                            if !self.claim_io_response(io_kind, generation) {
                                if !self.is_terminal() {
                                    log::error!(
                                        "tmux domain {} could not claim guarded response for \
                                         generation {generation}; detaching to preserve lease \
                                        ownership",
                                        self.domain_id
                                    );
                                }
                                if let Err(err) = self.signal_io_response(generation) {
                                    log::error!(
                                        "tmux domain {} could not retire unclaimed response I/O generation {generation}: {err}",
                                        self.domain_id
                                    );
                                }
                                self.abandon_command_result(
                                    split_failure_authority,
                                    &resp,
                                    generation,
                                    "guarded_response_claim_failed",
                                );
                                return None;
                            }
                            if let Err(err) = self.signal_io_response(generation) {
                                log::error!(
                                    "tmux domain {} could not cancel response deadline for \
                                     generation {generation}: {err}",
                                    self.domain_id
                                );
                                self.abandon_command_result(
                                    split_failure_authority,
                                    &resp,
                                    generation,
                                    "guarded_response_signal_failed",
                                );
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
                                .saturating_add(resp.output.capacity())
                                .saturating_add(
                                    conditional_commit
                                        .as_ref()
                                        .map_or(0, TmuxConditionalCommit::retained_bytes),
                                );
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
                                self.abandon_command_result(
                                    split_failure_authority,
                                    &resp,
                                    generation,
                                    "guarded_response_barrier_admission_failed",
                                );
                                return None;
                            }
                            return Some((cmd, resp, generation, conditional_commit));
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

    fn schedule_command_result(
        self: &Arc<Self>,
        cmd: Box<dyn TmuxCommand>,
        resp: Guarded,
        generation: u64,
        conditional_commit: Option<TmuxConditionalCommit>,
    ) {
        let domain_id = self.domain_id;
        let split_failure_authority = cmd.split_failure_authority();
        if !promise::spawn::is_scheduler_configured() {
            log::error!(
                "cannot process tmux command result for domain {domain_id}: no scheduler is \
                 configured; detaching the domain"
            );
            self.abandon_command_result(
                split_failure_authority,
                &resp,
                generation,
                "command_result_scheduler_unavailable",
            );
            return;
        }
        let Some(mux) = Mux::try_get() else {
            self.abandon_command_result(
                split_failure_authority,
                &resp,
                generation,
                "command_result_mux_unavailable",
            );
            return;
        };
        let Some(expected_domain) = mux.get_domain(domain_id) else {
            self.abandon_command_result(
                split_failure_authority,
                &resp,
                generation,
                "command_result_domain_unavailable",
            );
            return;
        };
        let Some(tmux_domain) = expected_domain.downcast_ref::<TmuxDomain>() else {
            self.abandon_command_result(
                split_failure_authority,
                &resp,
                generation,
                "command_result_domain_type_changed",
            );
            return;
        };
        if !std::ptr::eq(tmux_domain.inner.as_ref(), self.as_ref()) {
            self.abandon_command_result(
                split_failure_authority,
                &resp,
                generation,
                "command_result_domain_replaced",
            );
            return;
        }
        let expected_inner = Arc::clone(&tmux_domain.inner);
        let barrier_lease = ResponseBarrierLease {
            owner: Arc::clone(&expected_inner),
            abandoned_split_result: split_failure_authority
                .map(|authority| (authority, resp.clone(), generation)),
            completed: false,
        };
        promise::spawn::spawn_into_main_thread(async move {
            let mut barrier_lease = barrier_lease;
            let global_matches = Mux::try_get().is_some_and(|current| Arc::ptr_eq(&current, &mux));
            if !global_matches {
                barrier_lease.recover("command_result_global_mux_replaced");
                return;
            }
            let Some(current_domain) = mux.get_domain(domain_id) else {
                barrier_lease.recover("command_result_domain_removed");
                return;
            };
            if !current_domain.same_registration(&expected_domain) {
                barrier_lease.recover("command_result_registration_replaced");
                return;
            }
            if expected_inner.complete_command_response(cmd, &resp, generation, conditional_commit)
            {
                barrier_lease.abandoned_split_result = None;
                barrier_lease.completed = true;
            } else {
                barrier_lease.recover("command_result_not_committed");
            }
        })
        .detach();
    }

    /// Validate conditional result authority before invoking a command result
    /// callback. Identity locks are acquired before the mailbox, so a hot
    /// keypress producer never waits behind topology-lock acquisition.
    ///
    /// Result callbacks run on the serialized mux main-thread path. The commit
    /// below nevertheless revalidates after the callback; this first check
    /// prevents an intent or replacement that was already stale at dispatch
    /// time from performing any reconciliation side effect.
    fn conditional_result_is_current(
        &self,
        generation: u64,
        conditional_commit: &TmuxConditionalCommit,
    ) -> bool {
        if conditional_commit.io_generation() != generation {
            return false;
        }

        match conditional_commit {
            TmuxConditionalCommit::WindowLayout {
                lease,
                local_tab_id,
                ..
            } => {
                let TmuxConditionalCommitIntent::WindowLayout { window_id, .. } = &lease.intent
                else {
                    return false;
                };
                let gui_tabs = self.gui_tabs.lock();
                if !gui_tabs
                    .get(window_id)
                    .is_some_and(|local_tab| local_tab.tab_id == *local_tab_id)
                {
                    return false;
                }
                self.cmd_queue.lock().conditional_commit_is_current(lease)
            }
            TmuxConditionalCommit::PaneSize {
                lease,
                local_pane_id,
                local_tab_id,
                remote_window_id,
                ..
            } => {
                let TmuxConditionalCommitIntent::PaneSize { pane_id, .. } = &lease.intent else {
                    return false;
                };
                let _pane_registry = self.pane_registry.lock();
                let gui_tabs = self.gui_tabs.lock();
                if !gui_tabs
                    .get(remote_window_id)
                    .is_some_and(|local_tab| local_tab.tab_id == *local_tab_id)
                {
                    return false;
                }
                let remote_panes = self.remote_panes.lock();
                let Some(remote_pane) = remote_panes.get(pane_id) else {
                    return false;
                };
                let pane = remote_pane.lock();
                if pane.local_pane_id != *local_pane_id
                    || pane.window_id != *remote_window_id
                    || pane.pane_id != *pane_id
                {
                    return false;
                }
                self.cmd_queue.lock().conditional_commit_is_current(lease)
            }
        }
    }

    /// Publish a suppression-cache update only while the exact I/O
    /// generation, latest admitted intent, and local mirror identity captured
    /// by pure preparation all still match. Topology identity locks are taken
    /// first; the mailbox is acquired only for the final constant-time claim
    /// and mutation, with no I/O or callback under any of these locks.
    fn commit_conditional_success(
        &self,
        generation: u64,
        conditional_commit: TmuxConditionalCommit,
    ) -> bool {
        if conditional_commit.io_generation() != generation {
            metrics::counter!(
                "mux.tmux.conditional_commit.skipped",
                "reason" => "io_generation",
            )
            .increment(1);
            return false;
        }

        let committed = match conditional_commit {
            TmuxConditionalCommit::WindowLayout {
                lease,
                local_tab_id,
                ..
            } => {
                let (window_id, layout_csum) = match &lease.intent {
                    TmuxConditionalCommitIntent::WindowLayout {
                        window_id,
                        layout_csum,
                        ..
                    } => (*window_id, layout_csum.clone()),
                    TmuxConditionalCommitIntent::PaneSize { .. } => return false,
                };
                let mut gui_tabs = self.gui_tabs.lock();
                let Some(local_tab) = gui_tabs.get_mut(&window_id) else {
                    return false;
                };
                if local_tab.tab_id != local_tab_id {
                    false
                } else {
                    let mut cmd_queue = self.cmd_queue.lock();
                    if !cmd_queue.retire_conditional_commit_if_current(&lease) {
                        return false;
                    }
                    local_tab.layout_csum = layout_csum;
                    true
                }
            }
            TmuxConditionalCommit::PaneSize {
                lease,
                local_pane_id,
                local_tab_id,
                remote_window_id,
                ..
            } => {
                let (pane_id, rows, cols) = match &lease.intent {
                    TmuxConditionalCommitIntent::PaneSize {
                        pane_id,
                        rows,
                        cols,
                    } => (*pane_id, *rows, *cols),
                    TmuxConditionalCommitIntent::WindowLayout { .. } => return false,
                };
                let _pane_registry = self.pane_registry.lock();
                let gui_tabs = self.gui_tabs.lock();
                let tab_matches = gui_tabs
                    .get(&remote_window_id)
                    .is_some_and(|local_tab| local_tab.tab_id == local_tab_id);
                if !tab_matches {
                    false
                } else {
                    let remote_panes = self.remote_panes.lock();
                    let Some(remote_pane) = remote_panes.get(&pane_id) else {
                        return false;
                    };
                    let mut pane = remote_pane.lock();
                    if pane.local_pane_id != local_pane_id
                        || pane.window_id != remote_window_id
                        || pane.pane_id != pane_id
                    {
                        false
                    } else {
                        let mut cmd_queue = self.cmd_queue.lock();
                        if !cmd_queue.conditional_commit_is_current(&lease) {
                            return false;
                        }
                        pane.pane_width = u64::from(cols);
                        pane.pane_height = u64::from(rows);
                        let retired = cmd_queue.retire_conditional_commit_if_current(&lease);
                        if !retired {
                            return false;
                        }
                        cmd_queue.uncertain_remote_pane_sizes.remove(&pane_id);
                        true
                    }
                }
            }
        };

        metrics::counter!(
            "mux.tmux.conditional_commit.completed",
            "outcome" => if committed { "committed" } else { "identity_stale" },
        )
        .increment(1);
        #[cfg(test)]
        if committed {
            self.test_conditional_commits
                .fetch_add(1, Ordering::Relaxed);
        }
        committed
    }

    fn apply_command_result(
        &self,
        cmd: Box<dyn TmuxCommand>,
        response: &Guarded,
        generation: u64,
        conditional_commit: Option<TmuxConditionalCommit>,
    ) -> bool {
        let Some(_active_operation) = self.begin_protocol_operation() else {
            return false;
        };
        let split_transaction = cmd.is_split_transaction();
        let split_failure_authority = cmd.split_failure_authority();
        if self.lifecycle.lock().terminalizing && !split_transaction {
            metrics::counter!(
                "mux.tmux.command_result.skipped",
                "reason" => "split_cleanup_terminalizing",
            )
            .increment(1);
            return true;
        }
        let conditional_result_is_stale =
            conditional_commit
                .as_ref()
                .is_some_and(|conditional_commit| {
                    !self.conditional_result_is_current(generation, conditional_commit)
                });
        if conditional_result_is_stale && !response.error {
            if let Some(conditional_commit) = conditional_commit.as_ref() {
                if conditional_commit.io_generation() == generation {
                    self.cmd_queue
                        .lock()
                        .retire_conditional_commit_if_current(conditional_commit.lease());
                }
            }
            metrics::counter!(
                "mux.tmux.conditional_commit.skipped",
                "reason" => "stale_before_result",
            )
            .increment(1);
            return true;
        }
        match catch_recoverable(
            RecoverablePanicSite::MuxTmuxCallback,
            std::panic::AssertUnwindSafe(|| cmd.process_result(self.domain_id, response)),
        ) {
            Ok(Ok(())) if !response.error => {}
            Ok(Ok(())) if split_transaction => {
                log::error!(
                    "tmux split transaction in domain {} received a guarded protocol error",
                    self.domain_id
                );
                self.fail_split_transaction_authority(split_failure_authority.clone());
                self.request_terminal(false);
            }
            Ok(Ok(())) => {
                log::error!(
                    "tmux command in domain {} accepted a guarded protocol error; detaching",
                    self.domain_id
                );
                self.transition_to_exit_and_schedule_detach();
                return false;
            }
            Ok(Err(err)) if split_transaction => {
                log::error!("Tmux split transaction result error: {err}");
                self.fail_split_transaction_authority(split_failure_authority.clone());
                self.request_terminal(false);
            }
            Ok(Err(err)) => {
                log::error!("Tmux processing command result error: {err}");
                self.transition_to_exit_and_schedule_detach();
                return false;
            }
            Err(_) if split_transaction => {
                log::error!(
                    "Tmux split transaction callback panicked in domain {}; quarantining before teardown",
                    self.domain_id
                );
                self.fail_split_transaction_authority(split_failure_authority);
                self.request_terminal(false);
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
        if let Some(conditional_commit) = conditional_commit {
            self.commit_conditional_success(generation, conditional_commit);
        }
        true
    }

    fn complete_command_response(
        self: &Arc<Self>,
        cmd: Box<dyn TmuxCommand>,
        response: &Guarded,
        generation: u64,
        conditional_commit: Option<TmuxConditionalCommit>,
    ) -> bool {
        if self.apply_command_result(cmd, response, generation, conditional_commit) {
            self.protocol_barrier.lock().response_committed();
            self.drain_protocol_response_barrier();
            true
        } else {
            false
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
            abandoned_split_result: None,
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
            self.maybe_finish_terminalizing();
            if self.is_terminal() {
                return;
            }
            let _ = self.require_send_schedule("protocol response barrier drain");
            return;
        }
    }

    #[cfg(test)]
    fn process_command_result(&self, cmd: Box<dyn TmuxCommand>, response: &Guarded) {
        if !self.apply_command_result(cmd, response, 0, None) {
            return;
        }
        if self.transition_state(State::ProcessingResponse, State::Idle) {
            self.maybe_finish_terminalizing();
            let _ = self.require_send_schedule("test command-result completion");
        }
    }

    /// send next command at the front of cmd_queue.
    /// must be called inside main thread
    fn send_next_command(self: &Arc<Self>) {
        if let Err(err) = self.send_next_command_inner() {
            log::error!(
                "failed to transmit a tmux command for domain {}: {err:#}; failing the bounded sender lane",
                self.domain_id
            );
            self.fail_sender_operation("sender_admission_failed");
        }
    }

    fn send_next_command_inner(self: &Arc<Self>) -> anyhow::Result<()> {
        let Some(active_operation) = self.begin_owned_protocol_operation() else {
            return Ok(());
        };
        if !self.transition_state(State::Idle, State::Sending) {
            return Ok(());
        }

        // One main-thread turn inspects a fixed number of mailbox entries.
        // Retryable work parks on one explicit fact, so it cannot be selected
        // again until its exact lease is superseded or that fact is published.
        let mut preparation_budget = CMD_QUEUE_PREPARATION_QUANTUM;

        let (command, generation, io_kind) = loop {
            if preparation_budget == 0 {
                let lifecycle = self.lifecycle.lock();
                let _cmd_queue = self.cmd_queue.lock();
                let _ =
                    self.transition_state_with_lifecycle(&lifecycle, State::Sending, State::Idle);
                return Ok(());
            }
            let (prepared_command, conditional_commit_lease, superseded) = {
                let lifecycle = self.lifecycle.lock();
                let mut cmd_queue = self.cmd_queue.as_ref().lock();
                let prepared_command = match cmd_queue.take_next_for_sender_preparation() {
                    Some(Ok(command)) => Some(command),
                    Some(Err(stale)) => {
                        drop(cmd_queue);
                        drop(lifecycle);
                        drop(stale);
                        preparation_budget -= 1;
                        TmuxDomainState::wake_notification_intent_capacity(self.domain_id);
                        continue;
                    }
                    None => None,
                };
                let Some(mut prepared_command) = prepared_command else {
                    // Close the empty-check/Idle transition race under the
                    // mailbox lock. A later producer observes Idle and owns
                    // the next scheduling edge.
                    let _ = self.transition_state_with_lifecycle(
                        &lifecycle,
                        State::Sending,
                        State::Idle,
                    );
                    return Ok(());
                };
                let conditional_commit_lease = cmd_queue.prepared_conditional_commit();
                let superseded = cmd_queue.prepared_is_superseded();
                (prepared_command, conditional_commit_lease, superseded)
            };
            preparation_budget -= 1;

            if superseded {
                self.cmd_queue.lock().release_prepared();
                TmuxDomainState::wake_notification_intent_capacity(self.domain_id);
                continue;
            }

            // Command preparation can inspect mux/tab/pane state and may take
            // auxiliary locks. Do it outside the mailbox critical section so
            // keypress and resize producers never wait behind that work.
            let generation = self.alloc_io_generation()?;
            let retry_lease = conditional_commit_lease.clone();
            #[cfg(test)]
            self.test_command_preparations
                .fetch_add(1, Ordering::Relaxed);
            let preparation =
                prepared_command.prepare(self.domain_id, generation, conditional_commit_lease);
            match preparation {
                TmuxCommandPreparation::Ready {
                    command,
                    conditional_commit,
                } => {
                    let io_kind = if prepared_command.awaits_clean_exit() {
                        TmuxIoOperationKind::Detach
                    } else {
                        TmuxIoOperationKind::Command
                    };
                    let mut lifecycle = self.lifecycle.lock();
                    let mut cmd_queue = self.cmd_queue.as_ref().lock();
                    if !cmd_queue.prepared_install_authority_is_current(
                        generation,
                        conditional_commit.as_ref(),
                    ) {
                        cmd_queue.release_prepared();
                        drop(cmd_queue);
                        drop(lifecycle);
                        drop(prepared_command);
                        drop(command);
                        metrics::counter!(
                            "mux.tmux.conditional_commit.skipped",
                            "reason" => "stale_before_install",
                        )
                        .increment(1);
                        TmuxDomainState::wake_notification_intent_capacity(self.domain_id);
                        continue;
                    }
                    if !self.transition_state_with_lifecycle(
                        &lifecycle,
                        State::Sending,
                        State::WaitingForResponse,
                    ) {
                        cmd_queue.release_prepared();
                        drop(cmd_queue);
                        drop(lifecycle);
                        drop(prepared_command);
                        drop(command);
                        TmuxDomainState::wake_notification_intent_capacity(self.domain_id);
                        return Ok(());
                    }
                    if !cmd_queue.install_in_flight(
                        prepared_command,
                        generation,
                        conditional_commit,
                    ) {
                        anyhow::bail!(
                            "tmux command mailbox closed or already had an in-flight command during sender reservation"
                        );
                    }
                    if !Self::install_io_operation_locked(&mut lifecycle, io_kind, generation) {
                        anyhow::bail!(
                            "tmux domain {} could not install the unique I/O lease for generation \
                             {generation}",
                            self.domain_id,
                        );
                    }
                    break (command, generation, io_kind);
                }
                TmuxCommandPreparation::Suppressed | TmuxCommandPreparation::Discarded => {
                    anyhow::ensure!(
                        !prepared_command.is_split_transaction(),
                        "tmux split transaction was suppressed or discarded during sender preparation"
                    );
                    let mut cmd_queue = self.cmd_queue.lock();
                    if let Some(lease) = retry_lease.as_ref() {
                        cmd_queue.retire_conditional_commit_if_current(lease);
                    }
                    cmd_queue.release_prepared();
                    drop(cmd_queue);
                    TmuxDomainState::wake_notification_intent_capacity(self.domain_id);
                }
                TmuxCommandPreparation::Retryable { prerequisite } => {
                    anyhow::ensure!(
                        !prepared_command.is_split_transaction(),
                        "tmux split transaction became retryable during sender preparation"
                    );
                    let mut cmd_queue = self.cmd_queue.lock();
                    let retry_is_current = retry_lease
                        .as_ref()
                        .is_none_or(|lease| cmd_queue.conditional_commit_is_current(lease));
                    if retry_is_current {
                        anyhow::ensure!(
                            cmd_queue.restore_prepared_for_retry(prepared_command, prerequisite,),
                            "tmux retryable preparation lost its mailbox reservation"
                        );
                    } else {
                        cmd_queue.release_prepared();
                        drop(cmd_queue);
                        TmuxDomainState::wake_notification_intent_capacity(self.domain_id);
                    }
                }
            }
        };

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
            format!("admitting tmux command generation {generation} to the bounded I/O lane")
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
        if !scheduled_inner.should_schedule_send() {
            return Ok(());
        }
        if !scheduled_inner.try_claim_send_schedule() {
            return Ok(());
        }
        #[cfg(test)]
        scheduled_inner
            .test_send_runnables_scheduled
            .fetch_add(1, Ordering::Relaxed);
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

    /// Resolve the exact remote scope before allocating or admitting a split
    /// transaction.  The remote pane gate, not the local mirror census, is the
    /// authority for the session/window used by both baseline and recovery.
    fn resolve_tmux_split_target(
        &self,
        target: &PaneOperationGuard,
    ) -> anyhow::Result<(TmuxPaneId, TmuxSessionId, TmuxWindowId)> {
        let registered_domain = target.owner().get_domain(self.domain_id).ok_or_else(|| {
            anyhow::anyhow!(
                "tmux split target belongs to a mux without domain {}",
                self.domain_id
            )
        })?;
        let registered_tmux = registered_domain
            .downcast_ref::<TmuxDomain>()
            .context("tmux split target domain changed concrete type before command admission")?;
        anyhow::ensure!(
            target.admitted_domain_id() == self.domain_id
                && std::ptr::eq(registered_tmux.inner.as_ref(), self),
            "tmux split target domain {} changed exact identity before command admission",
            self.domain_id
        );
        let pane_id = target.pane_id();
        let tmux_pane_id = self.mirror_index.lock().remote_pane_for_local(pane_id);

        let id = tmux_pane_id.with_context(|| {
            format!("Could not find the tmux pane peer for local pane: {pane_id}")
        })?;
        let remote = self
            .remote_panes
            .lock()
            .get(&id)
            .cloned()
            .with_context(|| format!("tmux split target remote pane %{id} disappeared"))?;
        let remote = remote.lock();
        anyhow::ensure!(
            remote.local_pane_id == pane_id && remote.pane_id == id,
            "tmux split target remote pane %{id} changed exact local identity"
        );
        Ok((id, remote.session_id, remote.window_id))
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
            output_lane: OnceLock::new(),
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
            pane_registry: Mutex::new(()),
            retired_panes: Mutex::new(HashSet::new()),
            remote_split_reservations: Mutex::new(HashMap::default()),
            split_cleanup_obligations: Mutex::new(HashMap::default()),
            split_cleanup_quarantine: Mutex::new(VecDeque::new()),
            remote_split_identity_permits: AtomicUsize::new(0),
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
            test_conditional_commits: AtomicUsize::new(0),
            #[cfg(test)]
            test_command_preparations: AtomicUsize::new(0),
            #[cfg(test)]
            test_send_runnables_scheduled: AtomicUsize::new(0),
            #[cfg(test)]
            test_split_config_panic: AtomicU8::new(0),
            #[cfg(test)]
            test_split_output_failure: AtomicU8::new(0),
            #[cfg(test)]
            test_retire_split_domain_before_local_commit: AtomicBool::new(false),
            #[cfg(test)]
            test_io_deadlines: Mutex::new(None),
        });
        let io_lane = TmuxIoLane::new(domain_id, Arc::downgrade(&inner))
            .with_context(|| format!("cannot start tmux I/O supervisor for domain {domain_id}"))?;
        inner
            .io_lane
            .set(io_lane)
            .unwrap_or_else(|_| unreachable!("tmux I/O lane is initialized exactly once"));
        let output_lane = TmuxPaneOutputLane::new(
            domain_id,
            Arc::downgrade(&inner),
            TMUX_OUTPUT_ACTIVE_PANE_LIMIT,
        )?;
        inner
            .output_lane
            .set(output_lane)
            .unwrap_or_else(|_| unreachable!("tmux output lane is initialized exactly once"));
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
    fn supports_floating_pane_spawn(&self) -> bool {
        false
    }

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
            let (target_remote_pane_id, target_session_id, target_window_id) =
                self.inner.resolve_tmux_split_target(target)?;
            let pending = PendingTmuxSplit::new(
                &self.inner,
                request_id,
                promise,
                target_remote_pane_id,
                target_session_id,
                target_window_id,
                split_request.direction,
            )?;
            let cleanup = Arc::clone(&pending.cleanup);
            let cleanup_command: Box<dyn TmuxCommand> = Box::new(CompensateSplitPane {
                obligation: Arc::clone(&cleanup),
            });
            let baseline_command: Box<dyn TmuxCommand> = Box::new(SnapshotSplitPane::new(
                request_id,
                target_remote_pane_id,
                target_window_id,
            ));
            let admission = (|| -> anyhow::Result<Option<PendingTmuxSplit>> {
                let mut pending_splits = self.inner.pending_splits.lock();
                anyhow::ensure!(
                    pending_splits.len() < TMUX_PENDING_SPLIT_LIMIT,
                    "tmux pending split cap {TMUX_PENDING_SPLIT_LIMIT} exceeded"
                );
                anyhow::ensure!(
                    !pending_splits.contains_key(&request_id),
                    "duplicate tmux split request id {request_id}"
                );
                pending_splits
                    .try_reserve(1)
                    .map_err(|error| anyhow::anyhow!("reserve pending tmux split: {error}"))?;
                self.inner
                    .cmd_queue
                    .lock()
                    .admit_prepared_split(cleanup_command, baseline_command)?;
                Ok(pending_splits.insert(request_id, pending))
            })();
            let admitted = match admission {
                Ok(admitted) => admitted,
                Err(error) => {
                    cleanup.complete_without_remote_effect(
                        "tmux split admission failed before remote effect",
                    );
                    return Err(error);
                }
            };
            debug_assert!(admitted.is_none());
            if let Err(error) = self
                .inner
                .require_send_schedule("split-pane command admission")
            {
                let removed = self
                    .inner
                    .cmd_queue
                    .lock()
                    .remove_queued_pending_split(request_id);
                if !removed {
                    log::error!(
                        "tmux split request {request_id} lost its queued baseline after sender scheduling failed"
                    );
                }
                let _ = self.inner.fail_pending_split(
                    request_id,
                    anyhow::anyhow!(
                        "tmux split request {request_id} could not schedule its admitted command: {error:#}"
                    ),
                );
                return Err(error);
            }
            drop(active_operation);

            let remote = future
                .await
                .context("tmux split command did not produce a remote reservation")?;
            let _materialize_operation = self.inner.begin_active_operation().ok_or_else(|| {
                anyhow::anyhow!(
                    "tmux domain {} detached before split pane materialization",
                    self.inner.domain_id
                )
            })?;
            return self.inner.split_pane(mux, target, remote, split_request);
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
        anyhow::bail!("moving an existing pane into a tmux control-mode split is unsupported")
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

        if self.inner.cmd_queue.lock().split_cleanup_barrier {
            self.inner.transition_to_exit_and_schedule_detach();
            return Ok(());
        }

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
    use crate::tmux_commands::{Resize, SendKeys, SplitPane, TmuxCommand, TmuxCommandClass};
    use frankenterm_term::color::ColorPalette;
    use frankenterm_term::{KeyCode, KeyModifiers};
    use parking_lot::{MappedMutexGuard, MutexGuard};
    use promise::spawn::{ScopedExecutor, block_on};
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
    fn nonwrapping_atomic_u64_allocator_exhausts_without_mutation() {
        let counter = AtomicU64::new(u64::MAX - 1);

        assert_eq!(alloc_nonwrapping_atomic_u64(&counter), Some(u64::MAX - 1));
        assert_eq!(counter.load(Ordering::Acquire), u64::MAX);
        assert_eq!(alloc_nonwrapping_atomic_u64(&counter), None);
        assert_eq!(counter.load(Ordering::Acquire), u64::MAX);
    }

    #[test]
    fn nonwrapping_atomic_u64_allocator_is_unique_under_contention() {
        const WORKERS: usize = 8;
        const ALLOCATIONS_PER_WORKER: usize = 256;

        let counter = Arc::new(AtomicU64::new(1));
        let workers = (0..WORKERS)
            .map(|_| {
                let counter = Arc::clone(&counter);
                std::thread::spawn(move || {
                    (0..ALLOCATIONS_PER_WORKER)
                        .map(|_| {
                            alloc_nonwrapping_atomic_u64(&counter)
                                .expect("test allocation space must remain available")
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect::<Vec<_>>();

        let mut allocated = workers
            .into_iter()
            .flat_map(|worker| worker.join().expect("allocation worker must not panic"))
            .collect::<Vec<_>>();
        allocated.sort_unstable();

        let allocation_count = u64::try_from(WORKERS * ALLOCATIONS_PER_WORKER)
            .expect("test allocation count must fit u64");
        assert_eq!(allocated, (1..=allocation_count).collect::<Vec<_>>());
        assert_eq!(counter.load(Ordering::Acquire), allocation_count + 1);
    }

    #[test]
    fn pane_output_lane_rejection_rolls_back_only_its_reservation() {
        let active = Arc::new(AtomicUsize::new(1));
        let (ready, _receiver) = bounded(1);
        ready.try_send(41).expect("fill the bounded lane");
        let lane = TmuxPaneOutputLane {
            ready,
            active: Arc::clone(&active),
            connected: Arc::new(AtomicBool::new(true)),
            capacity: 2,
        };

        assert_eq!(lane.schedule(42), Err(TmuxPaneOutputGap::DrainLaneCapacity));
        assert_eq!(active.load(Ordering::Acquire), 1);

        let active = Arc::new(AtomicUsize::new(0));
        let (ready, receiver) = bounded(1);
        drop(receiver);
        let lane = TmuxPaneOutputLane {
            ready,
            active: Arc::clone(&active),
            connected: Arc::new(AtomicBool::new(false)),
            capacity: 1,
        };

        assert_eq!(lane.schedule(43), Err(TmuxPaneOutputGap::DrainLaneClosed));
        assert_eq!(active.load(Ordering::Acquire), 0);

        let active = Arc::new(AtomicUsize::new(usize::MAX));
        let (ready, _receiver) = bounded(1);
        let lane = TmuxPaneOutputLane {
            ready,
            active: Arc::clone(&active),
            connected: Arc::new(AtomicBool::new(true)),
            capacity: usize::MAX,
        };

        assert_eq!(lane.schedule(44), Err(TmuxPaneOutputGap::DrainLaneCapacity));
        assert_eq!(active.load(Ordering::Acquire), usize::MAX);
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
        assert!(
            tmux_domain
                .inner
                .transition_state(State::WaitForInitialGuard, State::Idle)
        );

        tmux_domain.inner.fail_initial_guard("test_late_deadline");

        assert_eq!(*tmux_domain.inner.state.lock(), State::Idle);
        assert!(!tmux_domain.inner.lifecycle.lock().terminal);
    }

    #[test]
    fn tmux_guarded_response_claim_fences_a_late_command_deadline() {
        let tmux_domain = new_tmux_domain(181);
        *tmux_domain.inner.state.lock() = State::WaitingForResponse;
        assert!(
            tmux_domain
                .inner
                .install_io_operation(TmuxIoOperationKind::Command, 41)
        );
        assert!(
            tmux_domain
                .inner
                .claim_io_response(TmuxIoOperationKind::Command, 41)
        );

        tmux_domain.inner.fail_tmux_io_operation(
            TmuxIoOperationKind::Command,
            41,
            "test_late_deadline",
        );

        assert_eq!(*tmux_domain.inner.state.lock(), State::ProcessingResponse);
        let lifecycle = tmux_domain.inner.lifecycle.lock();
        assert!(!lifecycle.terminal);
        assert!(lifecycle.io_operation.is_none());
    }

    #[test]
    fn tmux_detach_deadline_and_clean_exit_have_first_claim_authority() {
        let clean_winner = new_tmux_domain(182);
        *clean_winner.inner.state.lock() = State::WaitingForResponse;
        assert!(
            clean_winner
                .inner
                .install_io_operation(TmuxIoOperationKind::Detach, 51)
        );
        assert!(
            clean_winner
                .inner
                .claim_io_response(TmuxIoOperationKind::Detach, 51)
        );
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
        assert!(
            deadline_winner
                .inner
                .install_io_operation(TmuxIoOperationKind::Detach, 52)
        );
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
        assert!(
            !deadline_winner
                .inner
                .clean_exit_requested
                .load(Ordering::Acquire)
        );
    }

    fn wait_until(label: &str, mut predicate: impl FnMut() -> bool) {
        let deadline = Instant::now()
            .checked_add(Duration::from_secs(5))
            .expect("test deadline should fit Instant");
        while !predicate() {
            assert!(Instant::now() < deadline, "timed out waiting for {}", label);
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
        write_limit_once: Option<usize>,
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

    #[derive(Debug)]
    struct RetryableRequiredTestCommand {
        sequence: usize,
    }

    #[derive(Debug)]
    struct DiscardedRequiredTestCommand;

    struct BarrierConditionalTestCommand {
        pane_id: TmuxPaneId,
        entered: SyncSender<()>,
        release: Receiver<()>,
    }

    impl fmt::Debug for BarrierConditionalTestCommand {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("BarrierConditionalTestCommand")
                .field("pane_id", &self.pane_id)
                .finish_non_exhaustive()
        }
    }

    impl TmuxCommand for BarrierConditionalTestCommand {
        fn mailbox_class(&self) -> TmuxCommandClass {
            TmuxCommandClass::CoalescibleIntent
        }

        fn get_command(&self, _domain_id: DomainId) -> String {
            "barrier-conditional-test\n".to_string()
        }

        fn conditional_commit_intent(&self) -> Option<TmuxConditionalCommitIntent> {
            Some(TmuxConditionalCommitIntent::PaneSize {
                pane_id: self.pane_id,
                rows: 40,
                cols: 120,
            })
        }

        fn prepare(
            &mut self,
            _domain_id: DomainId,
            io_generation: u64,
            lease: Option<TmuxConditionalCommitLease>,
        ) -> TmuxCommandPreparation {
            self.entered
                .send(())
                .expect("test preparation barrier receiver");
            self.release
                .recv()
                .expect("test preparation barrier release");
            let lease = lease.expect("conditional barrier command lease");
            TmuxCommandPreparation::Ready {
                command: b"barrier-conditional-test\n".to_vec(),
                conditional_commit: Some(TmuxConditionalCommit::PaneSize {
                    io_generation,
                    lease,
                    local_pane_id: 1,
                    local_tab_id: 1,
                    remote_window_id: 1,
                }),
            }
        }

        fn process_result(&self, _domain_id: DomainId, _result: &Guarded) -> anyhow::Result<()> {
            Ok(())
        }
    }

    impl TmuxCommand for ClassedTestCommand {
        fn mailbox_class(&self) -> TmuxCommandClass {
            self.class
        }

        fn get_command(&self, _domain_id: DomainId) -> String {
            format!("test-{}\n", self.sequence)
        }

        fn process_result(&self, _domain_id: DomainId, _result: &Guarded) -> anyhow::Result<()> {
            Ok(())
        }
    }

    impl TmuxCommand for RetryableRequiredTestCommand {
        fn mailbox_class(&self) -> TmuxCommandClass {
            TmuxCommandClass::RequiredControl
        }

        fn get_command(&self, _domain_id: DomainId) -> String {
            format!("retryable-test-{}\n", self.sequence)
        }

        fn prepare(
            &mut self,
            _domain_id: DomainId,
            _io_generation: u64,
            _lease: Option<TmuxConditionalCommitLease>,
        ) -> TmuxCommandPreparation {
            TmuxCommandPreparation::Retryable {
                prerequisite: TmuxPreparationPrerequisite::Attach,
            }
        }

        fn process_result(&self, _domain_id: DomainId, _result: &Guarded) -> anyhow::Result<()> {
            Ok(())
        }
    }

    impl TmuxCommand for DiscardedRequiredTestCommand {
        fn mailbox_class(&self) -> TmuxCommandClass {
            TmuxCommandClass::RequiredControl
        }

        fn get_command(&self, _domain_id: DomainId) -> String {
            String::new()
        }

        fn prepare(
            &mut self,
            _domain_id: DomainId,
            _io_generation: u64,
            _lease: Option<TmuxConditionalCommitLease>,
        ) -> TmuxCommandPreparation {
            TmuxCommandPreparation::Discarded
        }

        fn process_result(&self, _domain_id: DomainId, _result: &Guarded) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn complete_next_mailbox_command(queue: &mut TmuxCmdQueue) -> Box<dyn TmuxCommand> {
        let command = queue
            .take_next_for_preparation()
            .expect("mailbox command should be ready");
        assert!(queue.install_in_flight(command, 1, None));
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

        fn process_result(&self, _domain_id: DomainId, _result: &Guarded) -> anyhow::Result<()> {
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
            if let Some(limit) = self.write_limit_once.take() {
                let written = limit.min(buf.len());
                self.bytes.extend_from_slice(&buf[..written]);
                return Ok(written);
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
                    write_limit_once: None,
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
                    write_limit_once: None,
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
                    write_limit_once: None,
                    write_error: Some(std::io::ErrorKind::BrokenPipe),
                }),
                mux_registration: Arc::new(crate::PaneRegistrationSlot::default()),
            })
        }

        fn new_with_partial_then_failing_writer(
            pane_id: PaneId,
            domain_id: DomainId,
            partial_bytes: usize,
        ) -> Arc<Self> {
            Arc::new(Self {
                pane_id,
                domain_id,
                keys: Mutex::new(Vec::new()),
                writes: Mutex::new(RecordingWriter {
                    bytes: Vec::new(),
                    write_threads: Vec::new(),
                    write_gate: None,
                    write_limit_once: Some(partial_bytes),
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
        backlog.append_owned_with_limits(1, b"hello ".to_vec(), limits);
        backlog.append_owned_with_limits(1, b"world".to_vec(), limits);

        assert_eq!(backlog.pane_bytes(1), Some(b"hello world".to_vec()));
        assert_eq!(backlog.total_bytes(), 11);
    }

    #[test]
    fn tmux_backlog_item_cap_records_a_gap_without_retaining_a_suffix() {
        let mut backlog = TmuxBacklog::default();
        let limits = TmuxBacklogLimits::with_item_expiry(32, 128, 8, 2, Duration::MAX);
        backlog.append_owned_with_limits(1, b"A".to_vec(), limits);
        backlog.append_owned_with_limits(1, b"B".to_vec(), limits);
        backlog.append_owned_with_limits(2, b"C".to_vec(), limits);

        assert!(backlog.pane_requires_resync(1));
        assert_eq!(backlog.pane_bytes(1), Some(Vec::new()));
        assert_eq!(backlog.pane_bytes(2), Some(b"C".to_vec()));
        assert_eq!(backlog.total_items(), 1);
        assert!(backlog.requires_recovery());
        assert_eq!(
            backlog.take(1),
            Some(TmuxBacklogDrain::ResyncRequired),
            "item pressure must preserve a typed gap instead of an arbitrary suffix",
        );
        assert!(!backlog.requires_recovery());
    }

    #[test]
    fn tmux_backlog_expiry_promotes_to_global_authoritative_recovery() {
        let mut backlog = TmuxBacklog::default();
        let limits = TmuxBacklogLimits::with_item_expiry(32, 128, 8, 8, Duration::from_millis(10));
        let started = Instant::now();
        backlog.append_owned_with_limits_at(1, b"old".to_vec(), limits, started);
        backlog.append_owned_with_limits_at(
            2,
            b"new".to_vec(),
            limits,
            started + Duration::from_millis(5),
        );

        backlog.refresh_limits_at(limits, started + Duration::from_millis(10));

        assert!(backlog.requires_global_resync());
        assert_eq!(backlog.expired_entries, 1);
        assert_eq!(backlog.len(), 0);
        assert_eq!(backlog.total_bytes(), 0);
        assert_eq!(backlog.total_items(), 0);
        assert_eq!(backlog.retained_byte_capacity(), 0);
    }

    #[test]
    fn tmux_materialized_output_ingress_enforces_atomic_byte_and_item_caps() {
        let byte_limits = TmuxPaneOutputLimits::new(4, 8, 2);
        let mut ingress = TmuxPaneOutputIngress::default();
        ingress
            .push_back(b"AB".to_vec(), byte_limits)
            .expect("first bounded chunk");
        assert_eq!(
            ingress.push_back(b"CDE".to_vec(), byte_limits),
            Err(TmuxPaneOutputGap::ByteLimit)
        );
        assert_eq!(ingress.queued_bytes(), 2);
        assert_eq!(ingress.chunks.len(), 1);

        let item_limits = TmuxPaneOutputLimits::new(32, 2, 2);
        ingress
            .push_back(b"C".to_vec(), item_limits)
            .expect("second bounded chunk");
        assert_eq!(
            ingress.push_back(b"D".to_vec(), item_limits),
            Err(TmuxPaneOutputGap::ItemLimit)
        );
        assert_eq!(ingress.queued_bytes(), 3);
        assert_eq!(ingress.chunks.len(), 2);
    }

    #[test]
    fn tmux_materialized_output_prepend_is_ordered_and_rejects_partial_drain() {
        let limits = TmuxPaneOutputLimits::new(32, 8, 4);
        let mut ingress = TmuxPaneOutputIngress::default();
        ingress
            .push_back(b"C".to_vec(), limits)
            .expect("queue post-publication byte");
        ingress
            .prepend(VecDeque::from([b"A".to_vec(), b"B".to_vec()]), limits)
            .expect("prepend complete pre-publication stream");
        let ordered = ingress
            .chunks
            .iter()
            .flat_map(|chunk| chunk.iter().copied())
            .collect::<Vec<_>>();
        assert_eq!(ordered, b"ABC");

        ingress.front_offset = 1;
        assert_eq!(
            ingress.prepend(VecDeque::from([b"late".to_vec()]), limits),
            Err(TmuxPaneOutputGap::InvalidState)
        );
        assert_eq!(ingress.queued_bytes(), 3);
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
        complete_protocol_response(&tmux_domain.inner, completed);
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
        complete_protocol_response(&tmux_domain.inner, completed);
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
        complete_protocol_response(&tmux_domain.inner, completed);
        wait_until("ordered tmux detach clean exit", || {
            *tmux_domain.inner.state.lock() == State::Exit
        });
        assert_eq!(launcher.recorded_writes(), b"count\ndetach\n".to_vec());
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
            let lifecycle = inner.lifecycle.lock();
            lifecycle.active_operations == 0 && lifecycle.resources_cleaned
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
        assert!(
            inner
                .cmd_queue
                .lock()
                .push_back(Box::new(ListCommands))
                .is_ok()
        );

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
        assert!(
            tmux_domain
                .inner
                .cmd_queue
                .lock()
                .push_back(Box::new(ListCommands))
                .is_ok()
        );

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
        assert!(
            tmux_domain
                .inner
                .cmd_queue
                .lock()
                .push_back(Box::new(ListCommands))
                .is_ok()
        );

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
        assert!(
            tmux_domain
                .inner
                .cmd_queue
                .lock()
                .push_back(Box::new(ListCommands))
                .is_ok()
        );

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
            abandoned_split_result: None,
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

        tmux_domain.inner.advance(Box::new(vec![
            Event::Guarded(successful_guarded_response()),
        ]));
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
        complete_protocol_response(&tmux_domain.inner, completed);
        wait_until("tmux detach clean-exit deadline", || {
            *tmux_domain.inner.state.lock() == State::Exit
        });
        assert_eq!(launcher.recorded_writes(), b"detach\n".to_vec());
        assert!(launcher.recorded_keys().is_empty());
    }

    #[test]
    fn tmux_split_reservation_promise_has_the_command_generation_response_deadline() {
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
        assert!(
            tmux_domain
                .inner
                .pending_splits
                .lock()
                .insert(
                    44,
                    PendingTmuxSplit::new_test(&tmux_domain.inner, 44, split_promise, 7)
                        .expect("reserve split-timeout identity"),
                )
                .is_none()
        );
        tmux_domain
            .inner
            .cmd_queue
            .lock()
            .push_split_transaction(Box::new(SplitPane::new(
                tmux_domain.domain_id(),
                7,
                SplitDirection::Horizontal,
                44,
            )))
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
            conditional_commit: None,
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

    fn complete_protocol_response(
        inner: &Arc<TmuxDomainState>,
        completed: (
            Box<dyn TmuxCommand>,
            Guarded,
            u64,
            Option<TmuxConditionalCommit>,
        ),
    ) {
        let (command, response, generation, conditional_commit) = completed;
        inner.complete_command_response(command, &response, generation, conditional_commit);
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
        complete_protocol_response(&tmux_domain.inner, completed);

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
        complete_protocol_response(&tmux_domain.inner, completed);

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
                conditional_commit: None,
            });
            queue.retained_by_class[TmuxCommandClass::RequiredControl.index()] = 1;
        }
        *tmux_domain.inner.state.lock() = State::WaitingForResponse;
        assert!(
            tmux_domain
                .inner
                .install_io_operation(TmuxIoOperationKind::Command, 1)
        );

        let completed = tmux_domain
            .inner
            .process_protocol_events(vec![
                Event::Guarded(successful_guarded_response()),
                Event::Guarded(successful_guarded_response()),
            ])
            .expect("the owned response must activate the protocol barrier");
        complete_protocol_response(&tmux_domain.inner, completed);

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

        assert!(
            tmux_domain
                .inner
                .cmd_queue
                .lock()
                .push_back(Box::new(ListCommands))
                .is_ok()
        );
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
        assert!(
            tmux_domain
                .inner
                .clean_exit_requested
                .load(Ordering::Acquire)
        );
        drop(active_operation);
        assert!(
            mux.get_domain(tmux_domain.domain_id()).is_none(),
            "dropping the final operation lease must complete clean removal"
        );
    }

    #[test]
    fn tmux_terminal_transition_is_reentrant_from_active_callback() {
        let tmux_domain = new_tmux_domain(97);
        assert!(
            tmux_domain
                .inner
                .with_active_lifecycle(|| {
                    tmux_domain.inner.transition_to_exit_and_schedule_detach();
                })
                .is_some()
        );

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
                TmuxTopologyBarrierEvent::Intent(TmuxNotificationIntent::WindowInvalidated(20)),
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
            .observe_topology_event(TopologyRevision::new(5), TmuxTopologyBarrierEvent::Barrier)
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
        index.register_test_pane(11, 101).expect("register pane");
        index.register_window(22, 202).expect("register window");
        assert_eq!(index.remote_pane_for_local(11), Some(101));
        assert_eq!(index.remote_window_for_local_tab(22), Some(202));

        assert!(index.register_test_pane(11, 102).is_err());
        assert!(index.register_test_pane(12, 101).is_err());
        assert!(index.register_window(22, 203).is_err());
        assert!(index.register_window(23, 202).is_err());

        assert_eq!(
            index.unregister_pane(101).expect("unregister pane"),
            Some(11)
        );
        assert_eq!(
            index.unregister_window(202).expect("unregister window"),
            Some(22)
        );
        assert_eq!(index.remote_pane_for_local(11), None);
        assert_eq!(index.remote_window_for_local_tab(22), None);
    }

    #[test]
    fn tmux_mirror_index_rejects_corruption_without_half_unregistering() {
        let mut index = TmuxMirrorIndex::default();
        index.pane_by_remote.insert(101, 11);
        index.pane_by_local.insert(11, 102);
        index.tab_by_remote_window.insert(201, 21);
        index.window_by_local_tab.insert(21, 202);

        assert!(index.unregister_pane(101).is_err());
        assert_eq!(index.pane_by_remote.get(&101), Some(&11));
        assert_eq!(index.pane_by_local.get(&11), Some(&102));

        assert!(index.unregister_window(201).is_err());
        assert_eq!(index.tab_by_remote_window.get(&201), Some(&21));
        assert_eq!(index.window_by_local_tab.get(&21), Some(&202));
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
        backlog.append_owned_with_limits(
            1,
            b"\x1b]8;;https://example.com\x1b\\text".to_vec(),
            limits,
        );

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
        backlog.append_owned_with_limits(1, b"abcdef".to_vec(), limits);
        backlog.append_owned_with_limits(2, b"ghij".to_vec(), limits);
        backlog.append_owned_with_limits(1, b"k".to_vec(), limits);

        assert_eq!(backlog.pane_bytes(1), Some(b"abcdefk".to_vec()));
        assert!(backlog.pane_requires_resync(2));
        assert_eq!(backlog.total_bytes(), 7);

        backlog.append_owned_with_limits(3, b"zz".to_vec(), limits);
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
            TmuxBacklogLimits::with_item_expiry(8, 32, 4, 0, Duration::MAX),
            TmuxBacklogLimits::with_item_expiry(8, 32, 4, 4, Duration::ZERO),
        ] {
            let mut backlog = TmuxBacklog::default();
            backlog.append_owned_with_limits(1, b"discarded".to_vec(), limits);
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
        backlog.append_owned_with_limits(1, b"0123456789".to_vec(), roomy);
        backlog.append_owned_with_limits(2, b"abcdef".to_vec(), roomy);
        assert_eq!(backlog.total_bytes(), 16);

        let tight = TmuxBacklogLimits::new(4, 6, 4);
        backlog.append_owned_with_limits(2, Vec::new(), tight);
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
                assert!(
                    queue
                        .push_back(Box::new(ClassedTestCommand {
                            class: TmuxCommandClass::TerminalControl,
                            sequence,
                        }))
                        .is_ok()
                );
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
        assert!(
            queue
                .push_back(Box::new(SendKeys {
                    pane: 7,
                    keys: b"kept".to_vec(),
                }))
                .is_ok()
        );
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
    fn close_moves_high_water_mailbox_storage_out_of_the_critical_section() {
        let mut queue = TmuxCmdQueue::new();
        for pane_id in 1..=128 {
            queue
                .push_back(Box::new(Resize {
                    pane_id,
                    size: portable_pty::PtySize {
                        rows: 40,
                        cols: 120,
                        pixel_width: 0,
                        pixel_height: 0,
                    },
                }))
                .expect("high-water conditional intent admission");
        }
        for _ in 0..128 {
            let command = queue
                .take_next_for_preparation()
                .expect("high-water intent enters preparation");
            assert!(
                queue.restore_prepared_for_retry(command, TmuxPreparationPrerequisite::Attach,)
            );
        }
        queue.advance_preparation_prerequisite(TmuxPreparationPrerequisite::Attach);
        queue.uncertain_remote_pane_sizes.extend(1..=128);
        queue
            .push_back(Box::new(ListCommands))
            .expect("retain a durable allocation at close");
        assert!(queue.ready_retry_deferred_intents.capacity() > 0);
        assert!(queue.retry_deferred_intents.capacity() > 0);
        assert!(queue.latest_conditional_commits.capacity() > 0);
        assert!(queue.uncertain_remote_pane_sizes.capacity() > 0);

        let teardown = queue.close();
        assert_eq!(queue.durable_entries.capacity(), 0);
        assert_eq!(queue.intent_entries.capacity(), 0);
        assert_eq!(queue.retry_deferred_durable.capacity(), 0);
        assert_eq!(queue.ready_retry_deferred_intents.capacity(), 0);
        assert_eq!(queue.retry_deferred_intents.capacity(), 0);
        assert_eq!(queue.latest_conditional_commits.capacity(), 0);
        assert_eq!(queue.queued_conditional_commits.capacity(), 0);
        assert_eq!(queue.uncertain_remote_pane_sizes.capacity(), 0);
        assert!(queue.is_empty());

        assert_eq!(teardown.retry_deferred_intents.len(), 128);
        assert_eq!(teardown.ready_retry_deferred_intents.len(), 128);
        assert!(teardown.ready_retry_deferred_intents.capacity() > 0);
        assert!(teardown.retry_deferred_intents.capacity() > 0);
        assert!(teardown.latest_conditional_commits.capacity() > 0);
        assert!(teardown.uncertain_remote_pane_sizes.capacity() > 0);
        drop(teardown);
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

        let required_count = CMD_QUEUE_CONTROL_RESERVED_SLOTS - CMD_QUEUE_TERMINAL_RESERVED_SLOTS;
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
        assert_eq!(
            next_lossless.get_command(0),
            format!("test-{}\n", CMD_QUEUE_DURABLE_SERVICE_QUANTUM)
        );
    }

    #[test]
    fn retryable_required_head_yields_to_intent_without_reordering_durable_fifo() {
        let mut queue = TmuxCmdQueue::new();
        queue
            .push_back(Box::new(RetryableRequiredTestCommand { sequence: 1 }))
            .expect("retryable durable head");
        queue
            .push_back(Box::new(ClassedTestCommand {
                class: TmuxCommandClass::RequiredControl,
                sequence: 2,
            }))
            .expect("later durable command");
        queue
            .push_back(Box::new(ClassedTestCommand {
                class: TmuxCommandClass::CoalescibleIntent,
                sequence: 3,
            }))
            .expect("ready intent");

        let retryable = queue
            .take_next_for_preparation()
            .expect("durable head enters preparation first");
        assert!(matches!(
            retryable.prepare(0, 1, None),
            TmuxCommandPreparation::Retryable { .. }
        ));
        assert!(queue.restore_prepared_for_retry(retryable, TmuxPreparationPrerequisite::Attach,));

        let intent = queue
            .take_next_for_preparation_with_policy(false)
            .expect("ready intent bypasses the parked durable head");
        assert_eq!(intent.get_command(0), "test-3\n");
        queue.release_prepared();
        drop(intent);
        assert!(
            queue.take_next_for_preparation_with_policy(false).is_none(),
            "later durable work must not overtake the parked FIFO head",
        );
        assert!(
            !queue.has_pending(),
            "a stable blocked head must remain dormant without hiding scheduled work",
        );

        queue.advance_preparation_prerequisite(TmuxPreparationPrerequisite::Attach);
        assert!(queue.has_pending());
        let retried = queue
            .take_retry_deferred_for_preparation()
            .expect("progress makes the durable head retryable")
            .expect("the exact durable generation remains current");
        assert_eq!(retried.get_command(0), "retryable-test-1\n");
        queue.release_prepared();
    }

    #[test]
    fn ready_retryable_required_head_has_bounded_service_amid_continuous_intents() {
        let mut queue = TmuxCmdQueue::new();
        queue
            .push_back(Box::new(RetryableRequiredTestCommand { sequence: 1 }))
            .expect("retryable required head");
        queue
            .push_back(Box::new(ClassedTestCommand {
                class: TmuxCommandClass::RequiredControl,
                sequence: 2,
            }))
            .expect("later durable command");

        let head = queue
            .take_next_for_sender_preparation()
            .expect("required head selection")
            .expect("required head is not stale");
        assert!(matches!(
            head.prepare(0, 1, None),
            TmuxCommandPreparation::Retryable { .. }
        ));
        assert!(queue.restore_prepared_for_retry(head, TmuxPreparationPrerequisite::Attach,));
        queue.advance_preparation_prerequisite(TmuxPreparationPrerequisite::Attach);

        for sequence in 0..CMD_QUEUE_RETRY_INTENT_SERVICE_QUANTUM {
            queue
                .push_back(Box::new(ClassedTestCommand {
                    class: TmuxCommandClass::CoalescibleIntent,
                    sequence: 10_000 + sequence,
                }))
                .expect("continuous intent admission");
            let intent = queue
                .take_next_for_sender_preparation()
                .expect("intent selection before bounded retry deadline")
                .expect("ordinary intent is not stale");
            assert_eq!(intent.mailbox_class(), TmuxCommandClass::CoalescibleIntent);
            queue.release_prepared();
        }

        queue
            .push_back(Box::new(ClassedTestCommand {
                class: TmuxCommandClass::CoalescibleIntent,
                sequence: 20_000,
            }))
            .expect("intent present at forced durable boundary");
        let retried_head = queue
            .take_next_for_sender_preparation()
            .expect("ready durable retry must receive bounded service")
            .expect("ready durable retry is not stale");
        assert_eq!(retried_head.get_command(0), "retryable-test-1\n");
        queue.release_prepared();

        let next_durable = queue
            .take_next_for_sender_preparation()
            .expect("later durable command follows retried FIFO head")
            .expect("later durable command is not stale");
        assert_eq!(next_durable.get_command(0), "test-2\n");
        queue.release_prepared();
    }

    #[test]
    fn ready_retryable_intent_has_bounded_service_amid_continuous_fresh_intents() {
        let mut queue = TmuxCmdQueue::new();
        queue
            .push_back(Box::new(Resize {
                pane_id: 77,
                size: portable_pty::PtySize {
                    rows: 40,
                    cols: 120,
                    pixel_width: 0,
                    pixel_height: 0,
                },
            }))
            .expect("retryable resize admission");
        let retryable = queue
            .take_next_for_sender_preparation()
            .expect("resize selection")
            .expect("resize lease is current");
        assert!(queue.restore_prepared_for_retry(retryable, TmuxPreparationPrerequisite::Attach,));
        queue.advance_preparation_prerequisite(TmuxPreparationPrerequisite::Attach);

        for sequence in 0..CMD_QUEUE_DURABLE_SERVICE_QUANTUM {
            queue
                .push_back(Box::new(ClassedTestCommand {
                    class: TmuxCommandClass::CoalescibleIntent,
                    sequence,
                }))
                .expect("continuous fresh intent admission");
            let fresh = queue
                .take_next_for_sender_preparation()
                .expect("fresh intent before bounded retry deadline")
                .expect("fresh intent is not stale");
            assert_eq!(fresh.get_command(0), format!("test-{sequence}\n"));
            queue.release_prepared();
        }

        queue
            .push_back(Box::new(ClassedTestCommand {
                class: TmuxCommandClass::CoalescibleIntent,
                sequence: usize::MAX,
            }))
            .expect("fresh intent at forced retry boundary");
        let retried = queue
            .take_next_for_sender_preparation()
            .expect("ready retryable intent receives bounded service")
            .expect("retryable intent lease remains current");
        assert_eq!(retried.as_resize().map(|(pane_id, _)| pane_id), Some(77),);
        queue.release_prepared();

        let fresh_after_retry = queue
            .take_next_for_sender_preparation()
            .expect("fresh intent remains queued after forced retry")
            .expect("fresh intent is not stale");
        assert_eq!(
            fresh_after_retry.get_command(0),
            format!("test-{}\n", usize::MAX)
        );
        queue.release_prepared();
    }

    #[test]
    fn identical_conditional_admissions_keep_exact_distinct_generations() {
        let mut queue = TmuxCmdQueue::new();
        let resize = || {
            Box::new(Resize {
                pane_id: 19,
                size: portable_pty::PtySize {
                    rows: 40,
                    cols: 120,
                    pixel_width: 0,
                    pixel_height: 0,
                },
            }) as Box<dyn TmuxCommand>
        };
        queue.push_back(resize()).expect("older resize admission");
        let older = queue
            .take_next_for_preparation()
            .expect("older resize preparation");
        let older_lease = queue
            .prepared_conditional_commit()
            .expect("older exact resize lease");

        queue
            .push_back(resize())
            .expect("identical newer resize admission");
        assert!(
            !queue.conditional_commit_is_current(&older_lease),
            "an identical later request must still supersede the older generation",
        );
        queue.release_prepared();
        drop(older);

        let newer = queue
            .take_next_for_preparation()
            .expect("newer resize preparation");
        let newer_lease = queue
            .prepared_conditional_commit()
            .expect("newer exact resize lease");
        assert!(newer_lease.generation > older_lease.generation);
        assert!(queue.conditional_commit_is_current(&newer_lease));
        queue.release_prepared();
        drop(newer);
    }

    #[test]
    fn merged_resize_dequeues_only_its_replacement_lease() {
        let mut queue = TmuxCmdQueue::new();
        let resize = |rows, cols| {
            Box::new(Resize {
                pane_id: 23,
                size: portable_pty::PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                },
            }) as Box<dyn TmuxCommand>
        };
        queue.push_back(resize(30, 100)).expect("older resize");
        let target = TmuxConditionalCommitTarget::PaneSize(23);
        let older_lease = queue
            .latest_conditional_commits
            .get(&target)
            .expect("older lease")
            .clone();
        queue
            .push_back(resize(40, 120))
            .expect("merged newer resize");
        let replacement_lease = queue
            .latest_conditional_commits
            .get(&target)
            .expect("replacement lease")
            .clone();
        assert!(replacement_lease.generation > older_lease.generation);
        assert_eq!(queue.intent_entries.len(), 1);
        assert_eq!(
            queue
                .queued_conditional_commits
                .get(&target)
                .expect("one queued replacement lease")
                .len(),
            1,
        );

        let merged = queue
            .take_next_for_sender_preparation()
            .expect("merged resize selection")
            .expect("merged resize is not stale");
        let dequeued_lease = queue
            .prepared_conditional_commit()
            .expect("exact merged lease");
        assert_eq!(dequeued_lease, replacement_lease);
        assert_eq!(
            merged.as_resize(),
            Some((
                23,
                portable_pty::PtySize {
                    rows: 40,
                    cols: 120,
                    pixel_width: 0,
                    pixel_height: 0,
                },
            )),
        );
        assert!(!queue.conditional_commit_is_current(&older_lease));
        assert!(queue.conditional_commit_is_current(&dequeued_lease));
        assert!(!queue.queued_conditional_commits.contains_key(&target));
        queue.release_prepared();
    }

    #[test]
    fn newer_same_target_admission_fences_unlocked_preparation_before_install() {
        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(Arc::clone(&mux));
        let tmux_domain = Arc::new(new_tmux_domain(408));
        let registered: Arc<dyn Domain> = tmux_domain.clone();
        mux.add_domain(&registered)
            .expect("register prepare-install fence test domain");
        let (entered_tx, entered_rx) = mpsc::sync_channel(0);
        let (release_tx, release_rx) = mpsc::sync_channel(0);
        tmux_domain
            .inner
            .cmd_queue
            .lock()
            .push_back(Box::new(BarrierConditionalTestCommand {
                pane_id: 88,
                entered: entered_tx,
                release: release_rx,
            }))
            .expect("older conditional command admission");
        *tmux_domain.inner.state.lock() = State::Idle;

        let sender = Arc::clone(&tmux_domain.inner);
        let sender_thread = std::thread::spawn(move || sender.send_next_command_inner());
        entered_rx
            .recv()
            .expect("older preparation reached the unlocked barrier");
        tmux_domain
            .inner
            .cmd_queue
            .lock()
            .push_back(Box::new(Resize {
                pane_id: 88,
                size: portable_pty::PtySize {
                    rows: 50,
                    cols: 140,
                    pixel_width: 0,
                    pixel_height: 0,
                },
            }))
            .expect("newer same-target admission during preparation");
        release_tx
            .send(())
            .expect("release older preparation barrier");
        sender_thread
            .join()
            .expect("sender thread must not panic")
            .expect("sender fence path must remain healthy");

        assert_eq!(*tmux_domain.inner.state.lock(), State::Idle);
        assert!(tmux_domain.inner.lifecycle.lock().io_operation.is_none());
        let queue = tmux_domain.inner.cmd_queue.lock();
        assert!(queue.in_flight.is_none());
        assert!(queue.preparing.is_none());
        assert_eq!(queue.retry_deferred_intents.len(), 1);
        let deferred = queue
            .retry_deferred_intents
            .get(&TmuxConditionalCommitTarget::PaneSize(88))
            .expect("newer resize remains parked for attach completion");
        assert_eq!(
            deferred.command.as_resize().map(|(_, size)| size),
            Some(portable_pty::PtySize {
                rows: 50,
                cols: 140,
                pixel_width: 0,
                pixel_height: 0,
            }),
        );
        assert_eq!(
            tmux_domain
                .inner
                .test_command_preparations
                .load(Ordering::Relaxed),
            2,
            "the stale preparation is rejected and only the newer intent prepares next",
        );
    }

    #[test]
    fn preparation_scan_yields_after_fixed_quantum() {
        let tmux_domain = new_tmux_domain(405);
        {
            let mut queue = tmux_domain.inner.cmd_queue.lock();
            for _ in 0..=CMD_QUEUE_PREPARATION_QUANTUM {
                queue
                    .push_back(Box::new(DiscardedRequiredTestCommand))
                    .expect("discarded preparation test command");
            }
        }
        *tmux_domain.inner.state.lock() = State::Idle;

        tmux_domain.inner.send_next_command();
        {
            let queue = tmux_domain.inner.cmd_queue.lock();
            assert_eq!(queue.len(), 1);
            assert!(queue.has_pending());
        }
        assert_eq!(*tmux_domain.inner.state.lock(), State::Idle);

        tmux_domain.inner.send_next_command();
        assert!(tmux_domain.inner.cmd_queue.lock().is_empty());
        assert_eq!(*tmux_domain.inner.state.lock(), State::Idle);
    }

    #[test]
    fn targeted_pane_progress_reprepares_only_its_parked_resize() {
        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(Arc::clone(&mux));
        let tmux_domain = Arc::new(new_tmux_domain(406));
        let registered: Arc<dyn Domain> = tmux_domain.clone();
        mux.add_domain(&registered)
            .expect("register parked-resize test domain");
        *tmux_domain.inner.attach_state.lock() = AttachState::Done;
        const PARKED_TARGETS: usize = 8;
        {
            let mut queue = tmux_domain.inner.cmd_queue.lock();
            for pane_id in 1..=PARKED_TARGETS {
                queue
                    .push_back(Box::new(Resize {
                        pane_id: TmuxPaneId::try_from(pane_id)
                            .expect("small test pane id fits tmux identity"),
                        size: portable_pty::PtySize {
                            rows: 40,
                            cols: 120,
                            pixel_width: 0,
                            pixel_height: 0,
                        },
                    }))
                    .expect("parked resize admission");
            }
        }
        *tmux_domain.inner.state.lock() = State::Idle;
        tmux_domain.inner.send_next_command();
        assert_eq!(
            tmux_domain
                .inner
                .test_command_preparations
                .load(Ordering::Relaxed),
            PARKED_TARGETS,
        );
        {
            let queue = tmux_domain.inner.cmd_queue.lock();
            assert_eq!(queue.retry_deferred_intents.len(), PARKED_TARGETS);
            for pane_id in 1..=PARKED_TARGETS {
                let target = TmuxConditionalCommitTarget::PaneSize(
                    TmuxPaneId::try_from(pane_id).expect("small pane id"),
                );
                let deferred = queue
                    .retry_deferred_intents
                    .get(&target)
                    .expect("one parked resize per target");
                assert_eq!(
                    deferred.retry_prerequisite,
                    TmuxPreparationPrerequisite::Pane(
                        TmuxPaneId::try_from(pane_id).expect("small pane id"),
                    ),
                );
                assert!(!deferred.retry_ready);
            }
            assert!(!queue.has_pending());
        }

        tmux_domain
            .inner
            .cmd_queue
            .lock()
            .advance_preparation_prerequisite(TmuxPreparationPrerequisite::Pane(5));
        tmux_domain.inner.send_next_command();
        assert_eq!(
            tmux_domain
                .inner
                .test_command_preparations
                .load(Ordering::Relaxed),
            PARKED_TARGETS + 1,
            "one pane publication must reprepare only its exact dependent target",
        );
        let queue = tmux_domain.inner.cmd_queue.lock();
        assert_eq!(queue.retry_deferred_intents.len(), PARKED_TARGETS);
        assert!(
            queue
                .retry_deferred_intents
                .values()
                .all(|deferred| !deferred.retry_ready)
        );
        assert!(!queue.has_pending());
    }

    #[test]
    fn nonfront_retry_target_uses_exact_deduplicated_ready_fifo_token() {
        let mut queue = TmuxCmdQueue::new();
        for pane_id in [31, 32, 33] {
            queue
                .push_back(Box::new(Resize {
                    pane_id,
                    size: portable_pty::PtySize {
                        rows: 40,
                        cols: 120,
                        pixel_width: 0,
                        pixel_height: 0,
                    },
                }))
                .expect("parked resize admission");
            let command = queue
                .take_next_for_preparation()
                .expect("resize enters preparation");
            assert!(
                queue.restore_prepared_for_retry(
                    command,
                    TmuxPreparationPrerequisite::Pane(pane_id),
                )
            );
        }
        assert_eq!(
            queue.retry_deferred_intent_head,
            Some(TmuxConditionalCommitTarget::PaneSize(31)),
        );
        assert_eq!(
            queue.retry_deferred_intent_tail,
            Some(TmuxConditionalCommitTarget::PaneSize(33)),
        );
        assert!(queue.ready_retry_deferred_intents.is_empty());

        let nonfront = TmuxPreparationPrerequisite::Pane(33);
        queue.advance_preparation_prerequisite(nonfront);
        queue.advance_preparation_prerequisite(nonfront);
        assert_eq!(
            queue.ready_retry_deferred_intents,
            VecDeque::from([TmuxConditionalCommitTarget::PaneSize(33)]),
            "repeated publication must not mint duplicate ready tokens",
        );
        assert_eq!(
            queue
                .retry_deferred_intents
                .values()
                .filter(|deferred| deferred.retry_ready)
                .count(),
            queue.ready_retry_deferred_intents.len(),
            "every ready dormant target must own exactly one FIFO token",
        );

        let retried = queue
            .take_retry_deferred_intent_for_preparation()
            .expect("nonfront target has a ready token")
            .expect("nonfront target lease remains current");
        assert_eq!(retried.as_resize().map(|(pane_id, _)| pane_id), Some(33));
        assert!(queue.ready_retry_deferred_intents.is_empty());
        assert!(
            !queue
                .retry_deferred_intents
                .contains_key(&TmuxConditionalCommitTarget::PaneSize(33))
        );
        assert_eq!(
            queue.retry_deferred_intent_head,
            Some(TmuxConditionalCommitTarget::PaneSize(31)),
        );
        assert_eq!(
            queue.retry_deferred_intent_tail,
            Some(TmuxConditionalCommitTarget::PaneSize(32)),
        );
        assert_eq!(
            queue
                .retry_deferred_intents
                .get(&TmuxConditionalCommitTarget::PaneSize(32))
                .and_then(|deferred| deferred.retry_intent_next),
            None,
            "unlinking a nonfront target must repair the dormant tail in O(1)",
        );
        queue.release_prepared();
        drop(retried);
    }

    #[test]
    fn same_target_resize_supersession_coalesces_behind_dormant_front() {
        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(Arc::clone(&mux));
        let tmux_domain = Arc::new(new_tmux_domain(407));
        let registered: Arc<dyn Domain> = tmux_domain.clone();
        mux.add_domain(&registered)
            .expect("register deferred-coalescing test domain");
        {
            let mut queue = tmux_domain.inner.cmd_queue.lock();
            for pane_id in [1, 2] {
                queue
                    .push_back(Box::new(Resize {
                        pane_id,
                        size: portable_pty::PtySize {
                            rows: 30,
                            cols: 100,
                            pixel_width: 0,
                            pixel_height: 0,
                        },
                    }))
                    .expect("initial parked resize admission");
            }
        }
        *tmux_domain.inner.state.lock() = State::Idle;
        tmux_domain.inner.send_next_command();
        let initial_b_lease = {
            let queue = tmux_domain.inner.cmd_queue.lock();
            assert_eq!(
                queue.retry_deferred_intent_head,
                Some(TmuxConditionalCommitTarget::PaneSize(1)),
            );
            assert_eq!(queue.retry_deferred_intents.len(), 2);
            queue
                .retry_deferred_intents
                .get(&TmuxConditionalCommitTarget::PaneSize(2))
                .and_then(|deferred| deferred.conditional_commit.clone())
                .expect("initial exact target-B lease")
        };

        for delta in 0_u16..128 {
            tmux_domain
                .inner
                .cmd_queue
                .lock()
                .push_back(Box::new(Resize {
                    pane_id: 2,
                    size: portable_pty::PtySize {
                        rows: 40 + delta,
                        cols: 120 + delta,
                        pixel_width: 0,
                        pixel_height: 0,
                    },
                }))
                .expect("same-target deferred resize supersession");
            tmux_domain
                .inner
                .schedule_send_next_command()
                .expect("dormant merge scheduling check");
        }

        assert_eq!(
            tmux_domain
                .inner
                .test_command_preparations
                .load(Ordering::Relaxed),
            2,
            "neither dormant target may be re-prepared by same-target admission",
        );
        assert_eq!(
            tmux_domain
                .inner
                .test_send_runnables_scheduled
                .load(Ordering::Relaxed),
            0,
            "same-target dormant merges must not enqueue no-op main-thread senders",
        );
        let queue = tmux_domain.inner.cmd_queue.lock();
        assert_eq!(queue.len(), 2, "supersession must not consume capacity");
        assert_eq!(
            queue.retry_deferred_intent_tail,
            Some(TmuxConditionalCommitTarget::PaneSize(2)),
        );
        assert_eq!(queue.retry_deferred_intents.len(), 2);
        assert!(
            queue
                .retry_deferred_intents
                .values()
                .all(|deferred| !deferred.retry_ready)
        );
        assert!(!queue.conditional_commit_is_current(&initial_b_lease));
        let current_b = queue
            .retry_deferred_intents
            .get(&TmuxConditionalCommitTarget::PaneSize(2))
            .expect("one coalesced target-B retry");
        let current_b_lease = current_b
            .conditional_commit
            .as_ref()
            .expect("coalesced retry retains exact current lease");
        assert!(current_b_lease.generation > initial_b_lease.generation);
        assert!(queue.conditional_commit_is_current(current_b_lease));
        assert_eq!(
            current_b.command.as_resize(),
            Some((
                2,
                portable_pty::PtySize {
                    rows: 167,
                    cols: 247,
                    pixel_width: 0,
                    pixel_height: 0,
                },
            )),
        );
        assert!(
            !queue
                .queued_conditional_commits
                .contains_key(&TmuxConditionalCommitTarget::PaneSize(2))
        );
        assert!(!queue.has_pending());
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

    fn take_conditional_command(
        queue: &mut TmuxCmdQueue,
        command: Box<dyn TmuxCommand>,
    ) -> (Box<dyn TmuxCommand>, TmuxConditionalCommitLease) {
        queue
            .push_back(command)
            .expect("conditional command should be admitted");
        let command = queue
            .take_next_for_preparation()
            .expect("conditional command should enter preparation");
        let lease = queue
            .prepared_conditional_commit()
            .expect("conditional command should own the latest intent lease");
        assert!(!queue.prepared_is_superseded());
        queue.release_prepared();
        (command, lease)
    }

    fn install_conditional_test_tab(
        mux: &Arc<Mux>,
        inner: &TmuxDomainState,
        remote_window_id: TmuxWindowId,
        layout_csum: &str,
    ) -> TabId {
        let tab = Arc::new(Tab::new(&TerminalSize::default()));
        mux.add_tab_no_panes(&tab)
            .expect("register conditional-commit test tab");
        let local_tab_id = tab.tab_id();
        assert!(
            inner
                .gui_tabs
                .lock()
                .insert(
                    remote_window_id,
                    TmuxTab {
                        tab_id: local_tab_id,
                        tmux_window_id: remote_window_id,
                        layout_csum: layout_csum.to_string(),
                        panes: HashSet::new(),
                    },
                )
                .is_none()
        );
        local_tab_id
    }

    fn install_conditional_test_pane(
        inner: &TmuxDomainState,
        remote_pane_id: TmuxPaneId,
        local_pane_id: PaneId,
        remote_window_id: TmuxWindowId,
        cols: u64,
        rows: u64,
    ) {
        let (_read, write) = filedescriptor::socketpair().expect("test pane socketpair");
        assert!(
            inner
                .remote_panes
                .lock()
                .insert(
                    remote_pane_id,
                    Arc::new(Mutex::new(TmuxRemotePane {
                        local_pane_id,
                        output_write: write,
                        child_state: Arc::new(TmuxChildState::new()),
                        session_id: 1,
                        window_id: remote_window_id,
                        pane_id: remote_pane_id,
                        cursor_x: 0,
                        cursor_y: 0,
                        pane_width: cols,
                        pane_height: rows,
                        pane_left: 0,
                        pane_top: 0,
                        output_state: TmuxPaneOutputState::Ready,
                        output_ingress: TmuxPaneOutputIngress::default(),
                    })),
                )
                .is_none()
        );
    }

    fn install_conditional_layout_in_flight(
        tmux_domain: &Arc<TmuxDomain>,
        generation: u64,
        remote_window_id: TmuxWindowId,
        layout_csum: &str,
    ) -> Arc<[u8]> {
        let (command, lease) = {
            let mut queue = tmux_domain.inner.cmd_queue.lock();
            queue
                .push_back(Box::new(ListAllPanes {
                    window_id: remote_window_id,
                    // Conditional authority belongs only to pruning layout
                    // refreshes. The one success-path caller retains a
                    // synthetic pane in its response so pruning does not
                    // remove the fixture tab.
                    prune: true,
                    layout_csum: layout_csum.to_string(),
                }))
                .expect("conditional boundary command admission");
            let command = queue
                .take_next_for_preparation()
                .expect("conditional boundary command preparation");
            let lease = queue
                .prepared_conditional_commit()
                .expect("conditional boundary command lease");
            (command, lease)
        };
        let TmuxCommandPreparation::Ready {
            command: command_bytes,
            conditional_commit: Some(conditional_commit),
        } = command.prepare(tmux_domain.domain_id(), generation, Some(lease))
        else {
            panic!("conditional boundary command should prepare immutable bytes");
        };
        assert!(tmux_domain.inner.cmd_queue.lock().install_in_flight(
            command,
            generation,
            Some(conditional_commit),
        ));
        *tmux_domain.inner.state.lock() = State::WaitingForResponse;
        assert!(
            tmux_domain
                .inner
                .install_io_operation(TmuxIoOperationKind::Command, generation)
        );
        command_bytes
    }

    #[test]
    fn list_panes_preparation_is_pure_and_matching_success_commits_checksum() {
        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(Arc::clone(&mux));
        let tmux_domain = Arc::new(new_tmux_domain(240));
        let registered: Arc<dyn Domain> = tmux_domain.clone();
        mux.add_domain(&registered)
            .expect("register conditional-commit test domain");
        let local_tab_id = install_conditional_test_tab(&mux, &tmux_domain.inner, 71, "old0");

        let (command, lease) = take_conditional_command(
            &mut tmux_domain.inner.cmd_queue.lock(),
            Box::new(ListAllPanes {
                window_id: 71,
                prune: true,
                layout_csum: "new1".to_string(),
            }),
        );
        let prepared = command.prepare(tmux_domain.domain_id(), 17, Some(lease.clone()));
        let TmuxCommandPreparation::Ready {
            command: command_bytes,
            conditional_commit: Some(conditional_commit),
        } = prepared
        else {
            panic!("changed layout should prepare immutable list-panes bytes");
        };
        assert!(
            command_bytes
                .windows(b"list-panes".len())
                .any(|window| window == b"list-panes")
        );
        assert_eq!(
            tmux_domain
                .inner
                .gui_tabs
                .lock()
                .get(&71)
                .expect("test tab")
                .layout_csum,
            "old0",
            "preparation must not publish the suppression checksum",
        );

        assert!(
            !tmux_domain
                .inner
                .commit_conditional_success(18, conditional_commit.clone())
        );
        assert_eq!(
            tmux_domain
                .inner
                .gui_tabs
                .lock()
                .get(&71)
                .expect("test tab")
                .layout_csum,
            "old0",
            "a mismatched I/O generation must leave the checksum retryable",
        );
        assert!(
            tmux_domain
                .inner
                .commit_conditional_success(17, conditional_commit)
        );
        let tab = tmux_domain.inner.gui_tabs.lock();
        let tab = tab.get(&71).expect("test tab");
        assert_eq!(tab.tab_id, local_tab_id);
        assert_eq!(tab.layout_csum, "new1");
    }

    #[test]
    fn mandatory_snapshot_survives_later_same_checksum_prune() {
        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(Arc::clone(&mux));
        let tmux_domain = Arc::new(new_tmux_domain(410));
        let registered: Arc<dyn Domain> = tmux_domain.clone();
        mux.add_domain(&registered)
            .expect("register mandatory-snapshot ordering domain");
        install_conditional_test_tab(&mux, &tmux_domain.inner, 92, "same");
        let mut queue = tmux_domain.inner.cmd_queue.lock();
        queue
            .push_back(Box::new(ListAllPanes {
                window_id: 92,
                prune: false,
                layout_csum: "same".to_string(),
            }))
            .expect("mandatory non-pruning snapshot admission");
        queue
            .push_back(Box::new(ListAllPanes {
                window_id: 92,
                prune: true,
                layout_csum: "same".to_string(),
            }))
            .expect("same-checksum pruning refresh admission");

        let mandatory = queue
            .take_next_for_sender_preparation()
            .expect("mandatory snapshot selection")
            .expect("mandatory snapshot is never stale");
        assert!(!queue.prepared_is_superseded());
        assert!(queue.prepared_conditional_commit().is_none());
        assert!(matches!(
            mandatory.prepare(tmux_domain.domain_id(), 91, None),
            TmuxCommandPreparation::Ready {
                conditional_commit: None,
                ..
            }
        ));
        queue.release_prepared();

        let prune = queue
            .take_next_for_sender_preparation()
            .expect("pruning refresh selection")
            .expect("pruning refresh lease is current");
        let prune_lease = queue
            .prepared_conditional_commit()
            .expect("pruning refresh owns checksum authority");
        assert!(!queue.prepared_is_superseded());
        assert!(matches!(
            prune.prepare(tmux_domain.domain_id(), 92, Some(prune_lease.clone())),
            TmuxCommandPreparation::Suppressed,
        ));
        assert!(queue.retire_conditional_commit_if_current(&prune_lease));
        queue.release_prepared();
    }

    #[test]
    fn mandatory_snapshot_cannot_regress_earlier_changed_checksum_prune() {
        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(Arc::clone(&mux));
        let tmux_domain = Arc::new(new_tmux_domain(411));
        let registered: Arc<dyn Domain> = tmux_domain.clone();
        mux.add_domain(&registered)
            .expect("register reverse snapshot-ordering domain");
        install_conditional_test_tab(&mux, &tmux_domain.inner, 93, "old0");
        let mut queue = tmux_domain.inner.cmd_queue.lock();
        queue
            .push_back(Box::new(ListAllPanes {
                window_id: 93,
                prune: true,
                layout_csum: "new1".to_string(),
            }))
            .expect("changed-checksum pruning refresh admission");
        queue
            .push_back(Box::new(ListAllPanes {
                window_id: 93,
                prune: false,
                layout_csum: "old0".to_string(),
            }))
            .expect("later mandatory non-pruning snapshot admission");

        let prune = queue
            .take_next_for_sender_preparation()
            .expect("pruning refresh selection")
            .expect("mandatory snapshot must not stale pruning authority");
        let prune_lease = queue
            .prepared_conditional_commit()
            .expect("pruning refresh owns checksum authority");
        assert!(!queue.prepared_is_superseded());
        let TmuxCommandPreparation::Ready {
            conditional_commit: Some(prune_commit),
            ..
        } = prune.prepare(tmux_domain.domain_id(), 93, Some(prune_lease))
        else {
            panic!("changed checksum must prepare pruning reconciliation");
        };
        queue.release_prepared();
        drop(queue);
        assert!(
            tmux_domain
                .inner
                .commit_conditional_success(93, prune_commit)
        );

        let mut queue = tmux_domain.inner.cmd_queue.lock();
        let mandatory = queue
            .take_next_for_sender_preparation()
            .expect("mandatory snapshot selection")
            .expect("mandatory snapshot is never stale");
        assert!(!queue.prepared_is_superseded());
        assert!(queue.prepared_conditional_commit().is_none());
        assert!(matches!(
            mandatory.prepare(tmux_domain.domain_id(), 94, None),
            TmuxCommandPreparation::Ready {
                conditional_commit: None,
                ..
            }
        ));
        queue.release_prepared();
        drop(queue);
        assert_eq!(
            tmux_domain
                .inner
                .gui_tabs
                .lock()
                .get(&93)
                .expect("test tab")
                .layout_csum,
            "new1",
            "mandatory snapshot must not republish its older discovery checksum",
        );
    }

    #[test]
    fn guarded_success_commits_exact_in_flight_layout_authority_end_to_end() {
        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(Arc::clone(&mux));
        let tmux_domain = Arc::new(new_tmux_domain(252));
        let registered: Arc<dyn Domain> = tmux_domain.clone();
        mux.add_domain(&registered)
            .expect("register exact-success boundary domain");
        install_conditional_test_tab(&mux, &tmux_domain.inner, 83, "old0");
        let retained_pane_id = 983;
        tmux_domain
            .inner
            .gui_tabs
            .lock()
            .get_mut(&83)
            .expect("conditional success test tab")
            .panes
            .insert(retained_pane_id);
        let command = install_conditional_layout_in_flight(&tmux_domain, 86, 83, "new1");
        assert!(!command.is_empty());

        let mut success = successful_guarded_response();
        success.output = format!("$1 @83 %{retained_pane_id} 0 0 0 80 24 0 0 1\n");

        let (command, response, generation, conditional_commit) = tmux_domain
            .inner
            .cmd_queue
            .lock()
            .record_in_flight_response(&success)
            .expect("guarded success completes the exact in-flight response");
        assert!(
            tmux_domain
                .inner
                .claim_io_response(TmuxIoOperationKind::Command, 86)
        );
        assert!(tmux_domain.inner.apply_command_result(
            command,
            &response,
            generation,
            conditional_commit,
        ));
        assert_eq!(
            tmux_domain
                .inner
                .gui_tabs
                .lock()
                .get(&83)
                .expect("matching tab survives reconciliation")
                .layout_csum,
            "new1",
        );
        assert!(
            tmux_domain
                .inner
                .gui_tabs
                .lock()
                .get_mut(&83)
                .expect("matching tab survives reconciliation")
                .panes
                .remove(&retained_pane_id)
        );
        assert_eq!(
            tmux_domain
                .inner
                .test_conditional_commits
                .load(Ordering::Relaxed),
            1,
        );
    }

    #[test]
    fn abandoned_prepared_authority_leaves_layout_retry_eligible() {
        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(Arc::clone(&mux));
        let tmux_domain = Arc::new(new_tmux_domain(241));
        let registered: Arc<dyn Domain> = tmux_domain.clone();
        mux.add_domain(&registered)
            .expect("register retry-boundary test domain");
        install_conditional_test_tab(&mux, &tmux_domain.inner, 72, "old0");

        let (command, lease) = take_conditional_command(
            &mut tmux_domain.inner.cmd_queue.lock(),
            Box::new(ListAllPanes {
                window_id: 72,
                prune: true,
                layout_csum: "new1".to_string(),
            }),
        );
        let prepared = command.prepare(tmux_domain.domain_id(), 31, Some(lease));
        assert!(matches!(&prepared, TmuxCommandPreparation::Ready { .. }));
        drop(prepared);
        assert_eq!(
            tmux_domain
                .inner
                .gui_tabs
                .lock()
                .get(&72)
                .expect("test tab")
                .layout_csum,
            "old0",
            "abandoning prepared bytes and authority must not publish the checksum",
        );

        let (retry, retry_lease) = take_conditional_command(
            &mut tmux_domain.inner.cmd_queue.lock(),
            Box::new(ListAllPanes {
                window_id: 72,
                prune: true,
                layout_csum: "new1".to_string(),
            }),
        );
        assert!(matches!(
            retry.prepare(tmux_domain.domain_id(), 32, Some(retry_lease)),
            TmuxCommandPreparation::Ready { .. }
        ));
    }

    #[test]
    fn launcher_acquisition_failure_cannot_commit_conditional_state() {
        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(Arc::clone(&mux));
        let tmux_domain = Arc::new(new_tmux_domain(247));
        let registered: Arc<dyn Domain> = tmux_domain.clone();
        mux.add_domain(&registered)
            .expect("register missing-launcher boundary domain");
        install_conditional_test_tab(&mux, &tmux_domain.inner, 78, "old0");
        let command = install_conditional_layout_in_flight(&tmux_domain, 81, 78, "new1");

        let outcome = execute_tmux_io_write(
            &Arc::downgrade(&tmux_domain.inner),
            &TmuxIoWriteJob {
                generation: 81,
                kind: TmuxIoOperationKind::Command,
                command: Some(command),
            },
        );
        assert!(matches!(&outcome, TmuxIoWriteOutcome::LauncherPaneGone));
        tmux_domain.inner.fail_tmux_io_operation(
            TmuxIoOperationKind::Command,
            81,
            outcome.reason_label(),
        );
        assert_eq!(*tmux_domain.inner.state.lock(), State::Exit);
        assert!(tmux_domain.inner.cmd_queue.lock().is_closed());
        assert_eq!(
            tmux_domain
                .inner
                .test_conditional_commits
                .load(Ordering::Relaxed),
            0,
        );
    }

    #[test]
    fn partial_then_failed_write_cannot_commit_conditional_state() {
        let default_domain: Arc<dyn Domain> =
            Arc::new(LocalDomain::new("tmux-conditional-partial-write").expect("local domain"));
        let mux = Arc::new(Mux::new(Some(Arc::clone(&default_domain))));
        let _guard = ScopedMux::install(Arc::clone(&mux));
        let launcher =
            RecordingPane::new_with_partial_then_failing_writer(248, default_domain.domain_id(), 3);
        let launcher_dyn: Arc<dyn Pane> = launcher.clone();
        mux.add_pane(&launcher_dyn).expect("add partial writer");
        let tmux_domain = Arc::new(new_tmux_domain(248));
        let registered: Arc<dyn Domain> = tmux_domain.clone();
        mux.add_domain(&registered)
            .expect("register partial-write boundary domain");
        install_conditional_test_tab(&mux, &tmux_domain.inner, 79, "old0");
        let command = install_conditional_layout_in_flight(&tmux_domain, 82, 79, "new1");

        let outcome = execute_tmux_io_write(
            &Arc::downgrade(&tmux_domain.inner),
            &TmuxIoWriteJob {
                generation: 82,
                kind: TmuxIoOperationKind::Command,
                command: Some(command),
            },
        );
        assert!(matches!(
            &outcome,
            TmuxIoWriteOutcome::Io { error_kind, .. }
                if *error_kind == std::io::ErrorKind::BrokenPipe
        ));
        assert_eq!(launcher.recorded_writes().len(), 3);
        tmux_domain.inner.fail_tmux_io_operation(
            TmuxIoOperationKind::Command,
            82,
            outcome.reason_label(),
        );
        assert_eq!(*tmux_domain.inner.state.lock(), State::Exit);
        assert_eq!(
            tmux_domain
                .inner
                .test_conditional_commits
                .load(Ordering::Relaxed),
            0,
        );
    }

    #[test]
    fn cancellation_and_timeout_cannot_commit_conditional_state() {
        {
            let mux = Arc::new(Mux::new(None));
            let _guard = ScopedMux::install(Arc::clone(&mux));
            let tmux_domain = Arc::new(new_tmux_domain(249));
            let registered: Arc<dyn Domain> = tmux_domain.clone();
            mux.add_domain(&registered)
                .expect("register cancellation boundary domain");
            install_conditional_test_tab(&mux, &tmux_domain.inner, 80, "old0");
            let command = install_conditional_layout_in_flight(&tmux_domain, 83, 80, "new1");
            drop(command);

            tmux_domain.inner.transition_to_exit_and_schedule_detach();
            assert_eq!(*tmux_domain.inner.state.lock(), State::Exit);
            assert_eq!(
                tmux_domain
                    .inner
                    .test_conditional_commits
                    .load(Ordering::Relaxed),
                0,
            );
        }

        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(Arc::clone(&mux));
        let tmux_domain = Arc::new(new_tmux_domain(250));
        let registered: Arc<dyn Domain> = tmux_domain.clone();
        mux.add_domain(&registered)
            .expect("register timeout boundary domain");
        install_conditional_test_tab(&mux, &tmux_domain.inner, 81, "old0");
        let command = install_conditional_layout_in_flight(&tmux_domain, 84, 81, "new1");
        drop(command);

        tmux_domain.inner.fail_tmux_io_operation(
            TmuxIoOperationKind::Command,
            84,
            "response_timeout",
        );
        assert_eq!(*tmux_domain.inner.state.lock(), State::Exit);
        assert_eq!(
            tmux_domain
                .inner
                .test_conditional_commits
                .load(Ordering::Relaxed),
            0,
        );
    }

    #[test]
    fn tmux_error_result_cannot_commit_conditional_state() {
        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(Arc::clone(&mux));
        let tmux_domain = Arc::new(new_tmux_domain(251));
        let registered: Arc<dyn Domain> = tmux_domain.clone();
        mux.add_domain(&registered)
            .expect("register tmux-error boundary domain");
        install_conditional_test_tab(&mux, &tmux_domain.inner, 82, "old0");
        let command = install_conditional_layout_in_flight(&tmux_domain, 85, 82, "new1");
        drop(command);
        let tmux_error = Guarded {
            error: true,
            timestamp: 0,
            number: 0,
            flags: 0,
            output: "injected tmux error".to_string(),
        };
        let (command, response, generation, conditional_commit) = tmux_domain
            .inner
            .cmd_queue
            .lock()
            .record_in_flight_response(&tmux_error)
            .expect("tmux error completes the owned response");
        assert!(
            tmux_domain
                .inner
                .claim_io_response(TmuxIoOperationKind::Command, 85)
        );

        assert!(!tmux_domain.inner.apply_command_result(
            command,
            &response,
            generation,
            conditional_commit,
        ));
        assert_eq!(*tmux_domain.inner.state.lock(), State::Exit);
        assert_eq!(
            tmux_domain
                .inner
                .test_conditional_commits
                .load(Ordering::Relaxed),
            0,
        );
    }

    #[test]
    fn stale_conditional_tmux_error_still_fails_closed() {
        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(Arc::clone(&mux));
        let tmux_domain = Arc::new(new_tmux_domain(409));
        let registered: Arc<dyn Domain> = tmux_domain.clone();
        mux.add_domain(&registered)
            .expect("register stale-error boundary domain");
        install_conditional_test_tab(&mux, &tmux_domain.inner, 91, "old0");

        let (older, older_lease) = take_conditional_command(
            &mut tmux_domain.inner.cmd_queue.lock(),
            Box::new(ListAllPanes {
                window_id: 91,
                prune: true,
                layout_csum: "old1".to_string(),
            }),
        );
        let TmuxCommandPreparation::Ready {
            conditional_commit: Some(older_commit),
            ..
        } = older.prepare(tmux_domain.domain_id(), 90, Some(older_lease))
        else {
            panic!("older layout command should prepare");
        };
        tmux_domain
            .inner
            .cmd_queue
            .lock()
            .push_back(Box::new(ListAllPanes {
                window_id: 91,
                prune: true,
                layout_csum: "new2".to_string(),
            }))
            .expect("newer layout intent supersedes result authority");

        let tmux_error = Guarded {
            error: true,
            timestamp: 0,
            number: 0,
            flags: 0,
            output: "injected stale-lease tmux error".to_string(),
        };
        assert!(!tmux_domain.inner.apply_command_result(
            older,
            &tmux_error,
            90,
            Some(older_commit),
        ));
        assert_eq!(*tmux_domain.inner.state.lock(), State::Exit);
        assert!(tmux_domain.inner.cmd_queue.lock().is_closed());
        assert_eq!(
            tmux_domain
                .inner
                .test_conditional_commits
                .load(Ordering::Relaxed),
            0,
        );
    }

    #[test]
    fn newer_layout_intent_and_replacement_tab_fence_late_success() {
        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(Arc::clone(&mux));
        let tmux_domain = Arc::new(new_tmux_domain(244));
        let registered: Arc<dyn Domain> = tmux_domain.clone();
        mux.add_domain(&registered)
            .expect("register layout-overlap test domain");
        install_conditional_test_tab(&mux, &tmux_domain.inner, 75, "old0");

        let (older, older_lease) = take_conditional_command(
            &mut tmux_domain.inner.cmd_queue.lock(),
            Box::new(ListAllPanes {
                window_id: 75,
                prune: true,
                layout_csum: "old1".to_string(),
            }),
        );
        let TmuxCommandPreparation::Ready {
            conditional_commit: Some(older_commit),
            ..
        } = older.prepare(tmux_domain.domain_id(), 61, Some(older_lease))
        else {
            panic!("older layout reconciliation should prepare");
        };

        let (newer, newer_lease) = take_conditional_command(
            &mut tmux_domain.inner.cmd_queue.lock(),
            Box::new(ListAllPanes {
                window_id: 75,
                prune: true,
                layout_csum: "new2".to_string(),
            }),
        );
        let TmuxCommandPreparation::Ready {
            conditional_commit: Some(newer_commit),
            ..
        } = newer.prepare(tmux_domain.domain_id(), 62, Some(newer_lease))
        else {
            panic!("newer layout reconciliation should prepare");
        };
        assert!(
            !tmux_domain
                .inner
                .commit_conditional_success(61, older_commit)
        );
        assert_eq!(
            tmux_domain
                .inner
                .gui_tabs
                .lock()
                .get(&75)
                .expect("test tab")
                .layout_csum,
            "old0",
        );

        let replacement = Arc::new(Tab::new(&TerminalSize::default()));
        mux.add_tab_no_panes(&replacement)
            .expect("register replacement test tab");
        let replacement_tab_id = replacement.tab_id();
        tmux_domain.inner.gui_tabs.lock().insert(
            75,
            TmuxTab {
                tab_id: replacement_tab_id,
                tmux_window_id: 75,
                layout_csum: "repl".to_string(),
                panes: HashSet::new(),
            },
        );
        assert!(tmux_domain.inner.apply_command_result(
            newer,
            &successful_guarded_response(),
            62,
            Some(newer_commit),
        ));
        let tabs = tmux_domain.inner.gui_tabs.lock();
        let replacement = tabs.get(&75).expect("replacement tab");
        assert_eq!(replacement.tab_id, replacement_tab_id);
        assert_eq!(replacement.layout_csum, "repl");
    }

    #[test]
    fn newer_layout_admission_fences_old_result_before_topology_reconciliation() {
        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(Arc::clone(&mux));
        let tmux_domain = Arc::new(new_tmux_domain(245));
        let registered: Arc<dyn Domain> = tmux_domain.clone();
        mux.add_domain(&registered)
            .expect("register pre-result-fence test domain");
        install_conditional_test_tab(&mux, &tmux_domain.inner, 76, "old0");
        let retained_pane_id = 986;
        tmux_domain
            .inner
            .gui_tabs
            .lock()
            .get_mut(&76)
            .expect("test tab")
            .panes
            .insert(retained_pane_id);

        let (older, older_lease) = take_conditional_command(
            &mut tmux_domain.inner.cmd_queue.lock(),
            Box::new(ListAllPanes {
                window_id: 76,
                prune: true,
                layout_csum: "old1".to_string(),
            }),
        );
        let TmuxCommandPreparation::Ready {
            conditional_commit: Some(older_commit),
            ..
        } = older.prepare(tmux_domain.domain_id(), 71, Some(older_lease))
        else {
            panic!("older layout reconciliation should prepare");
        };
        tmux_domain
            .inner
            .cmd_queue
            .lock()
            .push_back(Box::new(ListAllPanes {
                window_id: 76,
                prune: true,
                layout_csum: "new2".to_string(),
            }))
            .expect("newer layout intent admission");

        assert!(tmux_domain.inner.apply_command_result(
            older,
            &successful_guarded_response(),
            71,
            Some(older_commit),
        ));
        let tabs = tmux_domain.inner.gui_tabs.lock();
        let tab = tabs.get(&76).expect("stale result must not remove the tab");
        assert_eq!(tab.layout_csum, "old0");
        assert!(tab.panes.contains(&retained_pane_id));
        drop(tabs);
        assert!(
            !tmux_domain
                .inner
                .retired_panes
                .lock()
                .contains(&retained_pane_id),
            "stale result must not begin pane reconciliation before it is fenced",
        );
    }

    #[test]
    fn empty_resize_preparation_is_retryable_and_side_effect_free() {
        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(Arc::clone(&mux));
        let tmux_domain = Arc::new(new_tmux_domain(242));
        let registered: Arc<dyn Domain> = tmux_domain.clone();
        mux.add_domain(&registered)
            .expect("register empty-preparation test domain");
        install_conditional_test_tab(&mux, &tmux_domain.inner, 73, "same");
        install_conditional_test_pane(&tmux_domain.inner, 83, 183, 73, 80, 24);

        let resize = Resize {
            pane_id: 83,
            size: portable_pty::PtySize {
                rows: 40,
                cols: 120,
                pixel_width: 0,
                pixel_height: 0,
            },
        };
        {
            let mut queue = tmux_domain.inner.cmd_queue.lock();
            queue
                .push_back(Box::new(resize))
                .expect("empty resize should be admitted");
        }
        *tmux_domain.inner.state.lock() = State::Idle;
        tmux_domain.inner.send_next_command();
        {
            let queue = tmux_domain.inner.cmd_queue.lock();
            assert_eq!(queue.len(), 1, "retryable preparation must remain admitted");
            assert!(
                queue
                    .front()
                    .is_some_and(|command| command.as_resize().is_some())
            );
            assert!(queue.preparing.is_none());
            assert!(queue.intent_entries.is_empty());
            assert_eq!(queue.retry_deferred_intents.len(), 1);
            assert!(
                !queue.has_pending(),
                "blocked retry work must not self-schedule or wake on ordinary protocol ingress",
            );
        }
        tmux_domain.inner.send_next_command();
        assert_eq!(
            tmux_domain
                .inner
                .cmd_queue
                .lock()
                .retry_deferred_intents
                .len(),
            1
        );
        assert_eq!(*tmux_domain.inner.state.lock(), State::Idle);
        let pane = tmux_domain.inner.remote_panes.lock();
        let pane = pane.get(&83).expect("test pane").lock();
        assert_eq!((pane.pane_width, pane.pane_height), (80, 24));
    }

    #[test]
    fn permanent_resize_preparation_failures_release_intent_capacity() {
        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(Arc::clone(&mux));
        let tmux_domain = Arc::new(new_tmux_domain(246));
        let registered: Arc<dyn Domain> = tmux_domain.clone();
        mux.add_domain(&registered)
            .expect("register permanent-resize test domain");
        install_conditional_test_tab(&mux, &tmux_domain.inner, 77, "same");
        install_conditional_test_pane(&tmux_domain.inner, 87, 187, 77, 80, 24);
        *tmux_domain.inner.attach_state.lock() = AttachState::Done;

        tmux_domain
            .inner
            .cmd_queue
            .lock()
            .push_back(Box::new(Resize {
                pane_id: 87,
                size: portable_pty::PtySize {
                    rows: 40,
                    cols: 120,
                    pixel_width: 0,
                    pixel_height: 0,
                },
            }))
            .expect("unsupported resize admission");
        *tmux_domain.inner.state.lock() = State::Idle;
        tmux_domain.inner.send_next_command();
        assert!(
            tmux_domain.inner.cmd_queue.lock().is_empty(),
            "definitively unsupported resize capability must not poison the intent lane",
        );

        tmux_domain.inner.retired_panes.lock().insert(88);
        tmux_domain
            .inner
            .cmd_queue
            .lock()
            .push_back(Box::new(Resize {
                pane_id: 88,
                size: portable_pty::PtySize {
                    rows: 50,
                    cols: 140,
                    pixel_width: 0,
                    pixel_height: 0,
                },
            }))
            .expect("retired-pane resize admission");
        tmux_domain.inner.send_next_command();
        assert!(
            tmux_domain.inner.cmd_queue.lock().is_empty(),
            "retired pane resize must release bounded intent capacity",
        );
    }

    #[test]
    fn direct_window_close_wakes_resize_parked_before_pane_retirement() {
        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(Arc::clone(&mux));
        let tmux_domain = Arc::new(new_tmux_domain(253));
        let registered: Arc<dyn Domain> = tmux_domain.clone();
        mux.add_domain(&registered)
            .expect("register direct-retirement test domain");
        let local_tab_id = install_conditional_test_tab(&mux, &tmux_domain.inner, 90, "same");
        tmux_domain
            .inner
            .mirror_index
            .lock()
            .register_window(local_tab_id, 90)
            .expect("register test window mirror identity");
        tmux_domain
            .inner
            .gui_tabs
            .lock()
            .get_mut(&90)
            .expect("test window")
            .panes
            .insert(900);
        *tmux_domain.inner.attach_state.lock() = AttachState::Done;

        tmux_domain
            .inner
            .cmd_queue
            .lock()
            .push_back(Box::new(Resize {
                pane_id: 900,
                size: portable_pty::PtySize {
                    rows: 40,
                    cols: 120,
                    pixel_width: 0,
                    pixel_height: 0,
                },
            }))
            .expect("racing resize admission");
        *tmux_domain.inner.state.lock() = State::Idle;
        tmux_domain.inner.send_next_command();
        {
            let queue = tmux_domain.inner.cmd_queue.lock();
            assert_eq!(queue.retry_deferred_intents.len(), 1);
            assert!(
                !queue
                    .retry_deferred_intents
                    .values()
                    .next()
                    .expect("parked resize")
                    .retry_ready
            );
            assert!(!queue.has_pending());
        }

        // Keep the production ingress path from scheduling an asynchronous
        // sender so the retirement-to-retry boundary remains deterministic.
        *tmux_domain.inner.state.lock() = State::ProcessingResponse;
        assert!(
            tmux_domain
                .inner
                .process_protocol_events(vec![Event::WindowClose { window: 90 }])
                .is_none()
        );
        assert!(tmux_domain.inner.retired_panes.lock().contains(&900));
        {
            let queue = tmux_domain.inner.cmd_queue.lock();
            assert!(
                queue
                    .retry_deferred_intents
                    .values()
                    .next()
                    .expect("retirement-woken resize")
                    .retry_ready
            );
            assert!(queue.has_pending());
        }

        *tmux_domain.inner.state.lock() = State::Idle;
        tmux_domain.inner.send_next_command();
        assert!(
            tmux_domain.inner.cmd_queue.lock().is_empty(),
            "retirement progress must wake and discard the now-retired resize",
        );
        assert_eq!(
            tmux_domain
                .inner
                .test_command_preparations
                .load(Ordering::Relaxed),
            2,
        );
    }

    #[test]
    fn newer_resize_intent_and_replacement_pane_fence_late_success() {
        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(Arc::clone(&mux));
        let tmux_domain = Arc::new(new_tmux_domain(243));
        let registered: Arc<dyn Domain> = tmux_domain.clone();
        mux.add_domain(&registered)
            .expect("register overlap test domain");
        let local_tab_id = install_conditional_test_tab(&mux, &tmux_domain.inner, 74, "same");
        install_conditional_test_pane(&tmux_domain.inner, 84, 184, 74, 80, 24);
        *tmux_domain.inner.attach_state.lock() = AttachState::Done;
        tmux_domain
            .inner
            .support_commands
            .lock()
            .insert("resize-window".to_string(), String::new());

        let (older, older_lease) = take_conditional_command(
            &mut tmux_domain.inner.cmd_queue.lock(),
            Box::new(Resize {
                pane_id: 84,
                size: portable_pty::PtySize {
                    rows: 40,
                    cols: 120,
                    pixel_width: 0,
                    pixel_height: 0,
                },
            }),
        );
        let TmuxCommandPreparation::Ready {
            conditional_commit: Some(older_commit),
            ..
        } = older.prepare(tmux_domain.domain_id(), 51, Some(older_lease))
        else {
            panic!("older resize should prepare");
        };

        let (newer, newer_lease) = take_conditional_command(
            &mut tmux_domain.inner.cmd_queue.lock(),
            Box::new(Resize {
                pane_id: 84,
                size: portable_pty::PtySize {
                    rows: 50,
                    cols: 140,
                    pixel_width: 0,
                    pixel_height: 0,
                },
            }),
        );
        let TmuxCommandPreparation::Ready {
            conditional_commit: Some(newer_commit),
            ..
        } = newer.prepare(tmux_domain.domain_id(), 52, Some(newer_lease))
        else {
            panic!("newer resize should prepare");
        };

        assert!(
            !tmux_domain
                .inner
                .commit_conditional_success(51, older_commit)
        );
        {
            let panes = tmux_domain.inner.remote_panes.lock();
            let pane = panes.get(&84).expect("test pane").lock();
            assert_eq!((pane.pane_width, pane.pane_height), (80, 24));
        }
        assert!(
            tmux_domain
                .inner
                .commit_conditional_success(52, newer_commit)
        );
        {
            let panes = tmux_domain.inner.remote_panes.lock();
            let pane = panes.get(&84).expect("test pane").lock();
            assert_eq!((pane.pane_width, pane.pane_height), (140, 50));
        }

        let (replacement_fenced, replacement_lease) = take_conditional_command(
            &mut tmux_domain.inner.cmd_queue.lock(),
            Box::new(Resize {
                pane_id: 84,
                size: portable_pty::PtySize {
                    rows: 60,
                    cols: 160,
                    pixel_width: 0,
                    pixel_height: 0,
                },
            }),
        );
        let TmuxCommandPreparation::Ready {
            conditional_commit: Some(replacement_commit),
            ..
        } = replacement_fenced.prepare(tmux_domain.domain_id(), 53, Some(replacement_lease))
        else {
            panic!("replacement-fenced resize should prepare");
        };
        let replaced = tmux_domain.inner.remote_panes.lock().remove(&84);
        assert!(replaced.is_some(), "test must replace the prepared pane");
        drop(replaced);
        install_conditional_test_pane(&tmux_domain.inner, 84, 284, 74, 90, 30);
        assert!(tmux_domain.inner.apply_command_result(
            replacement_fenced,
            &successful_guarded_response(),
            53,
            Some(replacement_commit),
        ));
        let panes = tmux_domain.inner.remote_panes.lock();
        let replacement = panes.get(&84).expect("replacement pane").lock();
        assert_eq!(replacement.local_pane_id, 284);
        assert_eq!((replacement.pane_width, replacement.pane_height), (90, 30));
        drop(replacement);
        drop(panes);
        assert_eq!(
            tmux_domain
                .inner
                .gui_tabs
                .lock()
                .get(&74)
                .expect("test tab")
                .tab_id,
            local_tab_id,
        );
    }

    #[test]
    fn stale_resize_success_forces_a_b_a_dispatch_and_exact_success_clears_uncertainty() {
        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(Arc::clone(&mux));
        let tmux_domain = Arc::new(new_tmux_domain(411));
        let registered: Arc<dyn Domain> = tmux_domain.clone();
        mux.add_domain(&registered)
            .expect("register resize uncertainty test domain");
        install_conditional_test_tab(&mux, &tmux_domain.inner, 94, "same");
        install_conditional_test_pane(&tmux_domain.inner, 804, 1804, 94, 80, 24);
        *tmux_domain.inner.attach_state.lock() = AttachState::Done;
        tmux_domain
            .inner
            .support_commands
            .lock()
            .insert("resize-window".to_string(), String::new());

        let resize = |rows, cols| {
            Box::new(Resize {
                pane_id: 804,
                size: portable_pty::PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                },
            }) as Box<dyn TmuxCommand>
        };

        let (resize_b, resize_b_lease) = {
            let mut queue = tmux_domain.inner.cmd_queue.lock();
            queue
                .push_back(resize(40, 120))
                .expect("resize B admission");
            let command = queue
                .take_next_for_preparation()
                .expect("resize B enters preparation");
            let lease = queue
                .prepared_conditional_commit()
                .expect("resize B exact lease");
            (command, lease)
        };
        let TmuxCommandPreparation::Ready {
            command: resize_b_bytes,
            conditional_commit: Some(resize_b_commit),
        } = resize_b.prepare(tmux_domain.domain_id(), 71, Some(resize_b_lease))
        else {
            panic!("changed resize B must prepare immutable bytes");
        };
        assert!(!resize_b_bytes.is_empty());
        {
            let mut queue = tmux_domain.inner.cmd_queue.lock();
            assert!(queue.prepared_install_authority_is_current(71, Some(&resize_b_commit),));
            assert!(queue.install_in_flight(resize_b, 71, Some(resize_b_commit)));
            assert!(!queue.pane_size_suppression_is_trustworthy(804));
            queue
                .push_back(resize(24, 80))
                .expect("newer resize A admission after B reached the I/O lane");
        }

        let successful = successful_guarded_response();
        let stale_b_completion = {
            let mut queue = tmux_domain.inner.cmd_queue.lock();
            assert!(queue.record_in_flight_response(&successful).is_none());
            queue
                .record_in_flight_response(&successful)
                .expect("resize B completes after both guarded responses")
        };
        let (resize_b, response_b, generation_b, commit_b) = stale_b_completion;
        assert!(tmux_domain.inner.apply_command_result(
            resize_b,
            &response_b,
            generation_b,
            commit_b,
        ));
        {
            let queue = tmux_domain.inner.cmd_queue.lock();
            assert!(
                !queue.pane_size_suppression_is_trustworthy(804),
                "stale B success cannot make the cached A dimensions authoritative",
            );
        }
        {
            let panes = tmux_domain.inner.remote_panes.lock();
            let pane = panes.get(&804).expect("test pane").lock();
            assert_eq!((pane.pane_width, pane.pane_height), (80, 24));
        }

        let (resize_a, resize_a_lease) = {
            let mut queue = tmux_domain.inner.cmd_queue.lock();
            let command = queue
                .take_next_for_preparation()
                .expect("newest resize A enters preparation");
            let lease = queue
                .prepared_conditional_commit()
                .expect("newest resize A exact lease");
            (command, lease)
        };
        let TmuxCommandPreparation::Ready {
            command: resize_a_bytes,
            conditional_commit: Some(resize_a_commit),
        } = resize_a.prepare(tmux_domain.domain_id(), 72, Some(resize_a_lease))
        else {
            panic!("uncertain cached A dimensions must force resize A dispatch");
        };
        let resize_a_text = String::from_utf8_lossy(resize_a_bytes.as_ref());
        assert!(resize_a_text.contains("resize-pane -x 80 -y 24 -t %804"));
        {
            let mut queue = tmux_domain.inner.cmd_queue.lock();
            assert!(queue.prepared_install_authority_is_current(72, Some(&resize_a_commit),));
            assert!(queue.install_in_flight(resize_a, 72, Some(resize_a_commit)));
            assert!(!queue.pane_size_suppression_is_trustworthy(804));
        }

        let exact_a_completion = {
            let mut queue = tmux_domain.inner.cmd_queue.lock();
            assert!(queue.record_in_flight_response(&successful).is_none());
            queue
                .record_in_flight_response(&successful)
                .expect("exact resize A completes after both guarded responses")
        };
        let (resize_a, response_a, generation_a, commit_a) = exact_a_completion;
        assert!(tmux_domain.inner.apply_command_result(
            resize_a,
            &response_a,
            generation_a,
            commit_a,
        ));
        assert!(
            tmux_domain
                .inner
                .cmd_queue
                .lock()
                .pane_size_suppression_is_trustworthy(804),
            "only the exact current matching A success clears remote uncertainty",
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
        let conditional_commit = TmuxConditionalCommit::PaneSize {
            io_generation: 1,
            lease: queue
                .prepared_conditional_commit()
                .expect("resize should retain conditional commit authority"),
            local_pane_id: 0,
            local_tab_id: 0,
            remote_window_id: 0,
        };
        assert!(queue.install_in_flight(resize, 1, Some(conditional_commit)));
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
        let (completed, retained_response, generation, conditional_commit) = queue
            .record_in_flight_response(&second_success)
            .expect("the second response should complete the resize");
        assert_eq!(generation, 1);
        assert!(conditional_commit.is_some());
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

        let sentinel_head: Box<dyn TmuxCommand> = Box::new(SplitPane::new(
            domain_id,
            4242,
            SplitDirection::Horizontal,
            1,
        ));
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
            assert!(queue.install_in_flight(prepared, 1, None));
            assert_eq!(queue.len(), 1);
        }

        *inner.state.lock() = State::WaitingForResponse;

        for _ in 0..CMD_QUEUE_MAX_DEPTH - CMD_QUEUE_TERMINAL_RESERVED_SLOTS - 1 {
            let mut queue = inner.cmd_queue.lock();
            assert!(
                inner
                    .push_command_capped(&mut queue, Box::new(ListCommands))
                    .is_ok()
            );
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

        let distinctive_head: Box<dyn TmuxCommand> = Box::new(SplitPane::new(
            domain_id,
            8888,
            SplitDirection::Vertical,
            2,
        ));
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
            assert!(
                inner
                    .push_command_capped(&mut queue, Box::new(ListCommands))
                    .is_ok()
            );
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
    fn tmux_split_reservation_command_error_resolves_pending_with_error() {
        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(Arc::clone(&mux));

        let tmux_domain = Arc::new(new_tmux_domain(0));
        let domain: Arc<dyn Domain> = tmux_domain.clone();
        mux.add_domain(&domain)
            .expect("test tmux domain should register");

        let mut promise = promise::Promise::new();
        let future = promise.get_future().expect("split future");
        assert!(
            tmux_domain
                .inner
                .pending_splits
                .lock()
                .insert(
                    42,
                    PendingTmuxSplit::new_test(&tmux_domain.inner, 42, promise, 99)
                        .expect("reserve command-error split identity"),
                )
                .is_none()
        );

        let cmd = SplitPane::new(
            tmux_domain.domain_id(),
            99,
            SplitDirection::Horizontal,
            42,
        );
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
        assert_eq!(
            tmux_domain
                .inner
                .remote_split_identity_permits
                .load(Ordering::Acquire),
            0,
            "failed command must release its pre-command retained-ID permit"
        );
    }

    #[test]
    fn tmux_split_reservation_terminal_cleanup_fails_pending_future() {
        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(Arc::clone(&mux));

        let tmux_domain = Arc::new(new_tmux_domain(0));
        let domain: Arc<dyn Domain> = tmux_domain.clone();
        mux.add_domain(&domain)
            .expect("test tmux domain should register");

        let mut promise = promise::Promise::new();
        let future = promise.get_future().expect("split future");
        assert!(
            tmux_domain
                .inner
                .pending_splits
                .lock()
                .insert(
                    43,
                    PendingTmuxSplit::new_test(&tmux_domain.inner, 43, promise, 99)
                        .expect("reserve terminal-cleanup split identity"),
                )
                .is_none()
        );

        tmux_domain.inner.transition_to_clean_exit();

        let err = block_on(future).expect_err("terminal cleanup must fail the pending split");
        assert!(
            err.to_string().contains("split request 43"),
            "unexpected cancellation error: {:#}",
            err
        );
        assert!(tmux_domain.inner.pending_splits.lock().is_empty());
        assert_eq!(
            tmux_domain
                .inner
                .remote_split_identity_permits
                .load(Ordering::Acquire),
            0,
            "terminal cleanup must release pending retained-ID permits"
        );
    }

    fn install_atomic_split_test_domain(
        launcher_pane_id: PaneId,
    ) -> (ScopedMux, Arc<TmuxDomain>, Arc<RecordingPane>) {
        let default_domain: Arc<dyn Domain> = Arc::new(
            LocalDomain::new(&format!("tmux-atomic-split-{launcher_pane_id}"))
                .expect("atomic split local domain"),
        );
        let mux = Arc::new(Mux::new(Some(Arc::clone(&default_domain))));
        let guard = ScopedMux::install(Arc::clone(&mux));
        let launcher = RecordingPane::new(launcher_pane_id, default_domain.domain_id());
        let launcher_dyn: Arc<dyn Pane> = launcher.clone();
        mux.add_pane(&launcher_dyn)
            .expect("register atomic split launcher pane");
        let tmux_domain = Arc::new(new_tmux_domain(launcher_pane_id));
        let domain: Arc<dyn Domain> = tmux_domain.clone();
        mux.add_domain(&domain)
            .expect("register atomic split tmux domain");
        *tmux_domain.inner.state.lock() = State::Idle;
        (guard, tmux_domain, launcher)
    }

    fn complete_atomic_split_command(inner: &Arc<TmuxDomainState>, output: &str, error: bool) {
        let completed = inner
            .process_protocol_events(vec![Event::Guarded(Guarded {
                error,
                timestamp: 0,
                number: 0,
                flags: 0,
                output: output.to_string(),
            })])
            .expect("atomic split command must own its guarded response");
        complete_protocol_response(inner, completed);
    }

    #[test]
    fn tmux_atomic_publication_cancelled_waiter_retires_remote_identity_once() {
        let (_guard, tmux_domain, launcher) = install_atomic_split_test_domain(301);

        let mut promise = promise::Promise::new();
        let future = promise.get_future().expect("cancelled split future");
        drop(future);
        assert!(
            tmux_domain
                .inner
                .pending_splits
                .lock()
                .insert(
                    81,
                    PendingTmuxSplit::new_test(&tmux_domain.inner, 81, promise, 17)
                        .expect("reserve cancelled-waiter split identity"),
                )
                .is_none()
        );

        assert!(
            tmux_domain
                .inner
                .resolve_pending_split(81, 17, 18)
                .expect("resolve cancelled split response")
        );
        assert!(tmux_domain.inner.pending_splits.lock().is_empty());
        assert!(!tmux_domain.inner.remote_panes.lock().contains_key(&18));
        assert_eq!(
            tmux_domain
                .inner
                .remote_split_reservations
                .lock()
                .get(&18)
                .expect("cancelled split tombstone")
                .load()
                .expect("valid cancelled split state"),
            TmuxRemoteSplitState::Retired
        );
        tmux_domain.inner.send_next_command();
        wait_until("cancelled split compensation write", || {
            launcher.recorded_writes() == b"kill-pane -t %18\n"
        });
        complete_atomic_split_command(&tmux_domain.inner, "", false);
        assert_eq!(launcher.recorded_writes(), b"kill-pane -t %18\n");
        assert!(
            !tmux_domain.inner.is_terminal(),
            "guarded split compensation must preserve a healthy domain"
        );
    }

    #[test]
    fn tmux_atomic_publication_corrupted_reservation_writes_one_kill_then_terminalizes() {
        let (_guard, tmux_domain, launcher) = install_atomic_split_test_domain(302);

        let reservation = tmux_domain
            .inner
            .reserve_test_remote_split(83, 31, 32)
            .expect("reserve corrupted rollback split");
        let child_state = reservation.child_state();
        let replacement = Arc::new(TmuxRemoteSplitStateCell::new());
        let displaced = tmux_domain
            .inner
            .remote_split_reservations
            .lock()
            .insert(32, replacement)
            .expect("displace exact rollback reservation");
        assert!(Arc::ptr_eq(&displaced, &reservation.state));

        drop(reservation);

        assert!(!tmux_domain.inner.is_terminal());
        tmux_domain.inner.send_next_command();
        wait_until("corrupt split compensation write", || {
            launcher.recorded_writes() == b"kill-pane -t %32\n"
        });
        assert!(child_state.try_wait().is_none());
        complete_atomic_split_command(&tmux_domain.inner, "", false);
        assert!(tmux_domain.inner.is_terminal());
        assert!(child_state.try_wait().is_some());
        assert_eq!(launcher.recorded_writes(), b"kill-pane -t %32\n");
    }

    #[test]
    fn tmux_atomic_publication_terminal_after_response_writes_one_kill_before_teardown() {
        let (_guard, tmux_domain, launcher) = install_atomic_split_test_domain(303);

        let mut promise = promise::Promise::new();
        let future = promise.get_future().expect("terminal-gap split future");
        assert!(
            tmux_domain
                .inner
                .pending_splits
                .lock()
                .insert(
                    84,
                    PendingTmuxSplit::new_test(&tmux_domain.inner, 84, promise, 33)
                        .expect("reserve terminal-gap split identity"),
                )
                .is_none()
        );
        assert!(
            tmux_domain
                .inner
                .resolve_pending_split(84, 33, 34)
                .expect("resolve terminal-gap split response")
        );
        let reservation = block_on(future).expect("receive terminal-gap remote reservation");
        let child_state = reservation.child_state();

        tmux_domain.inner.transition_to_exit_and_schedule_detach();
        assert!(!tmux_domain.inner.is_terminal());
        drop(reservation);

        tmux_domain.inner.send_next_command();
        wait_until("terminal-gap split compensation write", || {
            launcher.recorded_writes() == b"kill-pane -t %34\n"
        });
        assert!(child_state.try_wait().is_none());
        complete_atomic_split_command(&tmux_domain.inner, "", false);
        assert!(tmux_domain.inner.is_terminal());
        assert!(child_state.try_wait().is_some());
        assert_eq!(launcher.recorded_writes(), b"kill-pane -t %34\n");
    }

    #[test]
    fn tmux_atomic_publication_split_compensation_preempts_detach_barrier_once() {
        let (_guard, tmux_domain, launcher) = install_atomic_split_test_domain(304);
        let reservation = tmux_domain
            .inner
            .reserve_test_remote_split(85, 35, 36)
            .expect("reserve detach-race split");
        tmux_domain
            .inner
            .cmd_queue
            .lock()
            .push_domain_detach(Box::new(DetachClient))
            .expect("install detach barrier before reservation rollback");

        drop(reservation);
        tmux_domain.inner.send_next_command();
        wait_until("detach-race split compensation write", || {
            launcher.recorded_writes() == b"kill-pane -t %36\n"
        });
        complete_atomic_split_command(&tmux_domain.inner, "", false);
        assert_eq!(launcher.recorded_writes(), b"kill-pane -t %36\n");
        assert!(
            tmux_domain
                .inner
                .cmd_queue
                .lock()
                .front()
                .is_some_and(|command| command.awaits_clean_exit()),
            "guarded compensation must leave the pre-existing detach as the next response owner"
        );
    }

    #[test]
    fn tmux_atomic_publication_trailing_noise_compensates_exact_witness_once() {
        let (_guard, tmux_domain, launcher) = install_atomic_split_test_domain(305);
        let mut promise = promise::Promise::new();
        let future = promise.get_future().expect("trailing-noise split future");
        assert!(
            tmux_domain
                .inner
                .pending_splits
                .lock()
                .insert(
                    86,
                    PendingTmuxSplit::new_test(&tmux_domain.inner, 86, promise, 37)
                        .expect("reserve trailing-noise split"),
                )
                .is_none()
        );
        SplitPane::new(
            tmux_domain.domain_id(),
            37,
            SplitDirection::Horizontal,
            86,
        )
        .process_result(
            tmux_domain.domain_id(),
            &Guarded {
                error: false,
                timestamp: 0,
                number: 0,
                flags: 0,
                output: "%38\nwarning\n".to_string(),
            },
        )
        .expect("valid split witness must enter exact compensation");
        assert!(block_on(future).is_err());

        tmux_domain.inner.send_next_command();
        wait_until("trailing-noise split compensation write", || {
            launcher.recorded_writes() == b"kill-pane -t %38\n"
        });
        complete_atomic_split_command(&tmux_domain.inner, "", false);
        assert_eq!(launcher.recorded_writes(), b"kill-pane -t %38\n");
        assert!(!tmux_domain.inner.is_terminal());
    }

    #[test]
    fn tmux_atomic_publication_abandoned_exact_result_writes_one_kill_before_teardown() {
        let (_guard, tmux_domain, launcher) = install_atomic_split_test_domain(310);
        let mut promise = promise::Promise::new();
        let future = promise.get_future().expect("abandoned-result split future");
        assert!(
            tmux_domain
                .inner
                .pending_splits
                .lock()
                .insert(
                    91,
                    PendingTmuxSplit::new_test(&tmux_domain.inner, 91, promise, 47)
                        .expect("reserve abandoned-result split"),
                )
                .is_none()
        );
        *tmux_domain.inner.state.lock() = State::ProcessingResponse;
        tmux_domain
            .inner
            .protocol_barrier
            .lock()
            .activate(128, Vec::new())
            .expect("activate abandoned split response barrier");
        let lost = ResponseBarrierLease {
            owner: Arc::clone(&tmux_domain.inner),
            abandoned_split_result: Some((
                TmuxSplitFailureAuthority::Pending {
                    request_id: 91,
                    target_pane_id: 47,
                },
                Guarded {
                    error: false,
                    timestamp: 0,
                    number: 0,
                    flags: 0,
                    output: "%48\n".to_string(),
                },
                0,
            )),
            completed: false,
        };

        drop(lost);

        assert!(block_on(future).is_err());
        assert!(!tmux_domain.inner.is_terminal());
        tmux_domain.inner.send_next_command();
        wait_until("abandoned-result split compensation write", || {
            launcher.recorded_writes() == b"kill-pane -t %48\n"
        });
        complete_atomic_split_command(&tmux_domain.inner, "", false);
        assert!(tmux_domain.inner.is_terminal());
        assert_eq!(launcher.recorded_writes(), b"kill-pane -t %48\n");
    }

    #[test]
    fn tmux_atomic_publication_reconciliation_kills_exact_set_difference_once() {
        let (_guard, tmux_domain, launcher) = install_atomic_split_test_domain(306);
        let mut promise = promise::Promise::new();
        let future = promise.get_future().expect("reconciled split future");
        assert!(
            tmux_domain
                .inner
                .pending_splits
                .lock()
                .insert(
                    87,
                    PendingTmuxSplit::new_test(&tmux_domain.inner, 87, promise, 39)
                        .expect("reserve reconciled split"),
                )
                .is_none()
        );
        SplitPane::new(
            tmux_domain.domain_id(),
            39,
            SplitDirection::Vertical,
            87,
        )
        .process_result(
            tmux_domain.domain_id(),
            &Guarded {
                error: false,
                timestamp: 0,
                number: 0,
                flags: 0,
                output: "unparseable split output".to_string(),
            },
        )
        .expect("ambiguous split must enter reconciliation");

        tmux_domain.inner.send_next_command();
        wait_until("split reconciliation list-panes write", || {
            launcher.recorded_writes() == b"list-panes -a -F '#{pane_id}'\n"
        });
        complete_atomic_split_command(&tmux_domain.inner, "%40\n", false);
        assert!(block_on(future).is_err());
        tmux_domain.inner.send_next_command();
        wait_until("reconciled split compensation write", || {
            launcher.recorded_writes() == b"list-panes -a -F '#{pane_id}'\nkill-pane -t %40\n"
        });
        complete_atomic_split_command(&tmux_domain.inner, "", false);
        assert!(tmux_domain.inner.is_terminal());
        assert_eq!(
            launcher.recorded_writes(),
            b"list-panes -a -F '#{pane_id}'\nkill-pane -t %40\n"
        );
    }

    #[test]
    fn tmux_atomic_publication_concurrent_new_panes_are_quarantined_without_kill() {
        let (_guard, tmux_domain, launcher) = install_atomic_split_test_domain(307);
        let mut promise = promise::Promise::new();
        let future = promise.get_future().expect("ambiguous split future");
        assert!(
            tmux_domain
                .inner
                .pending_splits
                .lock()
                .insert(
                    88,
                    PendingTmuxSplit::new_test(&tmux_domain.inner, 88, promise, 41)
                        .expect("reserve ambiguous split"),
                )
                .is_none()
        );
        SplitPane::new(
            tmux_domain.domain_id(),
            41,
            SplitDirection::Horizontal,
            88,
        )
        .process_result(
            tmux_domain.domain_id(),
            &Guarded {
                error: false,
                timestamp: 0,
                number: 0,
                flags: 0,
                output: "%42\n%43\n".to_string(),
            },
        )
        .expect("multiple split identities must enter reconciliation");

        tmux_domain.inner.send_next_command();
        wait_until("ambiguous split reconciliation write", || {
            launcher.recorded_writes() == b"list-panes -a -F '#{pane_id}'\n"
        });
        complete_atomic_split_command(&tmux_domain.inner, "%42\n%43\n", false);
        assert!(block_on(future).is_err());
        assert!(tmux_domain.inner.is_terminal());
        assert_eq!(
            launcher.recorded_writes(),
            b"list-panes -a -F '#{pane_id}'\n",
            "ambiguous reconciliation must never kill either candidate"
        );
        let quarantine = tmux_domain.inner.split_cleanup_quarantine.lock();
        assert!(
            quarantine
                .iter()
                .any(|entry| { entry.request_id == 88 && entry.candidates == vec![42, 43] })
        );
    }

    #[test]
    fn tmux_atomic_publication_reconciliation_timeout_releases_permit_and_quarantines() {
        let (_guard, tmux_domain, launcher) = install_atomic_split_test_domain(308);
        *tmux_domain.inner.test_io_deadlines.lock() = Some(TmuxIoDeadlines {
            start: Duration::from_secs(1),
            write: Duration::from_secs(1),
            response: Duration::from_millis(25),
        });
        let mut promise = promise::Promise::new();
        let future = promise.get_future().expect("timeout split future");
        assert!(
            tmux_domain
                .inner
                .pending_splits
                .lock()
                .insert(
                    89,
                    PendingTmuxSplit::new_test(&tmux_domain.inner, 89, promise, 44)
                        .expect("reserve timeout split"),
                )
                .is_none()
        );
        SplitPane::new(
            tmux_domain.domain_id(),
            44,
            SplitDirection::Vertical,
            89,
        )
        .process_result(
            tmux_domain.domain_id(),
            &Guarded {
                error: false,
                timestamp: 0,
                number: 0,
                flags: 0,
                output: "missing identity".to_string(),
            },
        )
        .expect("invalid split output must enter bounded reconciliation");
        tmux_domain.inner.send_next_command();
        wait_until("split reconciliation timeout", || {
            tmux_domain.inner.is_terminal()
        });

        assert!(block_on(future).is_err());
        assert_eq!(
            launcher.recorded_writes(),
            b"list-panes -a -F '#{pane_id}'\n"
        );
        assert!(tmux_domain.inner.pending_splits.lock().is_empty());
        assert_eq!(
            tmux_domain
                .inner
                .remote_split_identity_permits
                .load(Ordering::Acquire),
            0
        );
        assert!(
            tmux_domain
                .inner
                .split_cleanup_obligations
                .lock()
                .is_empty()
        );
        assert!(
            tmux_domain
                .inner
                .split_cleanup_quarantine
                .lock()
                .iter()
                .any(|entry| entry.request_id == 89)
        );
    }

    #[test]
    fn tmux_atomic_publication_compensation_timeout_writes_once_and_quarantines() {
        let (_guard, tmux_domain, launcher) = install_atomic_split_test_domain(309);
        *tmux_domain.inner.test_io_deadlines.lock() = Some(TmuxIoDeadlines {
            start: Duration::from_secs(1),
            write: Duration::from_secs(1),
            response: Duration::from_millis(25),
        });
        let reservation = tmux_domain
            .inner
            .reserve_test_remote_split(90, 45, 46)
            .expect("reserve compensation-timeout split");
        let cleanup = reservation.cleanup_obligation();
        drop(reservation);
        tmux_domain.inner.send_next_command();
        wait_until("split compensation timeout", || {
            tmux_domain.inner.is_terminal()
        });

        assert_eq!(launcher.recorded_writes(), b"kill-pane -t %46\n");
        assert_eq!(cleanup.status(), TmuxSplitCleanupStatus::Failed);
        assert!(
            tmux_domain
                .inner
                .split_cleanup_obligations
                .lock()
                .is_empty()
        );
        assert!(
            tmux_domain
                .inner
                .split_cleanup_quarantine
                .lock()
                .iter()
                .any(|entry| entry.request_id == 90 && entry.candidates == vec![46])
        );
    }

    #[test]
    fn tmux_atomic_publication_retained_identity_cap_rejects_before_command_or_remote_mutation() {
        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(Arc::clone(&mux));
        let tmux_domain = Arc::new(new_tmux_domain(0));
        let domain: Arc<dyn Domain> = tmux_domain.clone();
        mux.add_domain(&domain)
            .expect("test tmux domain should register");
        {
            let mut retired = tmux_domain.inner.retired_panes.lock();
            retired
                .try_reserve(RETIRED_PANE_TOMBSTONE_LIMIT)
                .expect("reserve saturated retained-ID negative control");
            retired.extend((0..RETIRED_PANE_TOMBSTONE_LIMIT).map(|id| {
                TmuxPaneId::try_from(id).expect("retained-ID test range must fit tmux pane ids")
            }));
        }

        let command_admitted = AtomicBool::new(false);
        let error = tmux_domain
            .inner
            .with_reserved_remote_split_identity(|| {
                command_admitted.store(true, Ordering::Release);
                Ok(())
            })
            .expect_err("saturated retained-ID cap must reject pre-command admission");
        assert!(format!("{error:#}").contains("before split command admission"));
        assert!(
            !command_admitted.load(Ordering::Acquire),
            "retained-ID saturation must reject before invoking remote command admission"
        );
        assert_eq!(tmux_domain.inner.cmd_queue.lock().len(), 0);
        assert!(tmux_domain.inner.pending_splits.lock().is_empty());
        assert!(tmux_domain.inner.remote_panes.lock().is_empty());
        assert!(
            tmux_domain
                .inner
                .remote_split_reservations
                .lock()
                .is_empty()
        );
        assert_eq!(
            tmux_domain
                .inner
                .remote_split_identity_permits
                .load(Ordering::Acquire),
            0
        );
        assert!(
            tmux_domain
                .inner
                .split_cleanup_obligations
                .lock()
                .is_empty()
        );
    }

    #[test]
    fn tmux_atomic_publication_remote_id_collision_preserves_incumbent() {
        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(Arc::clone(&mux));
        let tmux_domain = Arc::new(new_tmux_domain(0));
        let domain: Arc<dyn Domain> = tmux_domain.clone();
        mux.add_domain(&domain)
            .expect("test tmux domain should register");
        let (_output_read, output_write) =
            filedescriptor::socketpair().expect("collision test socketpair");
        let incumbent = Arc::new(Mutex::new(TmuxRemotePane {
            local_pane_id: 777,
            output_write,
            child_state: Arc::new(TmuxChildState::new()),
            session_id: 1,
            window_id: 2,
            pane_id: 28,
            cursor_x: 0,
            cursor_y: 0,
            pane_width: 80,
            pane_height: 24,
            pane_left: 0,
            pane_top: 0,
            output_state: TmuxPaneOutputState::Ready,
            output_ingress: TmuxPaneOutputIngress::default(),
        }));
        tmux_domain
            .inner
            .mirror_index
            .lock()
            .register_test_pane(777, 28)
            .expect("register incumbent split identity");
        assert!(
            tmux_domain
                .inner
                .remote_panes
                .lock()
                .insert(28, Arc::clone(&incumbent))
                .is_none()
        );
        let mut promise = promise::Promise::new();
        let future = promise.get_future().expect("colliding split future");
        assert!(
            tmux_domain
                .inner
                .pending_splits
                .lock()
                .insert(
                    82,
                    PendingTmuxSplit::new_test(&tmux_domain.inner, 82, promise, 27)
                        .expect("reserve collision split identity"),
                )
                .is_none()
        );

        let error = tmux_domain
            .inner
            .resolve_pending_split(82, 27, 28)
            .expect_err("incumbent remote identity must reject split publication");
        assert!(format!("{error:#}").contains("already-materialized remote pane id 28"));
        assert_eq!(incumbent.lock().local_pane_id, 777);
        assert!(
            !tmux_domain
                .inner
                .remote_split_reservations
                .lock()
                .contains_key(&28)
        );
        assert!(block_on(future).is_err());
        assert!(tmux_domain.inner.is_terminal());
    }

    #[test]
    fn tmux_split_reservation_results_resolve_exact_request_identity_out_of_order() {
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
            assert!(
                pending
                    .insert(
                        100,
                        PendingTmuxSplit::new_test(&tmux_domain.inner, 100, first_promise, 8)
                            .expect("reserve first out-of-order split identity"),
                    )
                    .is_none()
            );
            assert!(
                pending
                    .insert(
                        200,
                        PendingTmuxSplit::new_test(&tmux_domain.inner, 200, second_promise, 9)
                            .expect("reserve second out-of-order split identity"),
                    )
                    .is_none()
            );
        }

        let second = SplitPane::new(
            tmux_domain.domain_id(),
            9,
            SplitDirection::Vertical,
            200,
        );
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

        let first = SplitPane::new(
            tmux_domain.domain_id(),
            8,
            SplitDirection::Horizontal,
            100,
        );
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

        let first_reservation = block_on(first_future).expect("first split reservation");
        let second_reservation = block_on(second_future).expect("second split reservation");
        assert_eq!(first_reservation.remote_pane_id(), 11);
        assert_eq!(second_reservation.remote_pane_id(), 22);
        assert!(tmux_domain.inner.pending_splits.lock().is_empty());
    }
}
