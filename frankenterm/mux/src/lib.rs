#![allow(clippy::bool_assert_comparison)]
#![allow(clippy::borrow_deref_ref)]
#![allow(clippy::box_collection)]
#![allow(clippy::boxed_local)]
#![allow(clippy::clone_on_copy)]
#![allow(clippy::collapsible_else_if)]
#![allow(clippy::derivable_impls)]
#![allow(clippy::empty_line_after_doc_comments)]
#![allow(clippy::explicit_auto_deref)]
#![allow(clippy::extra_unused_type_parameters)]
#![allow(clippy::field_reassign_with_default)]
#![allow(clippy::from_over_into)]
#![allow(clippy::get_first)]
#![allow(clippy::into_iter_on_ref)]
#![allow(clippy::io_other_error)]
#![allow(clippy::iter_kv_map)]
#![allow(clippy::iter_nth)]
#![allow(clippy::let_unit_value)]
#![allow(clippy::manual_flatten)]
#![allow(clippy::manual_map)]
#![allow(clippy::map_clone)]
#![allow(clippy::map_entry)]
#![allow(clippy::match_like_matches_macro)]
#![allow(clippy::needless_borrow)]
#![allow(clippy::needless_borrowed_reference)]
#![allow(clippy::needless_borrows_for_generic_args)]
#![allow(clippy::needless_return)]
#![allow(clippy::neg_multiply)]
#![allow(clippy::new_ret_no_self)]
#![allow(clippy::new_without_default)]
#![allow(clippy::nonminimal_bool)]
#![allow(clippy::option_as_ref_deref)]
#![allow(clippy::option_map_unit_fn)]
#![allow(clippy::redundant_field_names)]
#![allow(clippy::redundant_guards)]
#![allow(clippy::redundant_pattern)]
#![allow(clippy::redundant_pattern_matching)]
#![allow(clippy::result_large_err)]
#![allow(clippy::search_is_some)]
#![allow(clippy::single_char_add_str)]
#![allow(clippy::single_match)]
#![allow(clippy::too_many_arguments)]
#![allow(clippy::type_complexity)]
#![allow(clippy::unnecessary_cast)]
#![allow(clippy::unnecessary_get_then_check)]
#![allow(clippy::unnecessary_lazy_evaluations)]
#![allow(clippy::unnecessary_map_or)]
#![allow(clippy::useless_conversion)]
#![allow(clippy::useless_format)]
#![allow(clippy::wildcard_in_or_patterns)]
#[cfg(all(feature = "async-smol", feature = "async-asupersync"))]
compile_error!(
    "mux async runtime features are mutually exclusive; enable only one of \"async-smol\" or \"async-asupersync\""
);
#[cfg(not(any(feature = "async-smol", feature = "async-asupersync")))]
compile_error!(
    "mux requires one async runtime feature: \"async-asupersync\" (preferred) or \"async-smol\""
);

use crate::client::{ClientId, ClientInfo};
use crate::pane::{CachePolicy, Pane, PaneId};
use crate::ssh_agent::AgentProxy;
use crate::tab::{SplitRequest, Tab, TabId};
use crate::tmux::TmuxDomain;
use crate::window::{Window, WindowId};
use anyhow::{anyhow, Context, Error};
use config::keyassignment::SpawnTabDomain;
use config::{configuration, ExitBehavior, GuiPosition};
use domain::{Domain, DomainId, DomainState, SplitSource};
use filedescriptor::{poll, pollfd, socketpair, AsRawSocketDescriptor, FileDescriptor, POLLIN};
use frankenterm_term::{Clipboard, ClipboardSelection, DownloadHandler, TerminalSize};
#[cfg(unix)]
use libc::{c_int, SOL_SOCKET, SO_RCVBUF, SO_SNDBUF};
use log::error;
use metrics::histogram;
use parking_lot::{
    MappedRwLockReadGuard, MappedRwLockWriteGuard, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard,
};
use percent_encoding::percent_decode_str;
use portable_pty::{CommandBuilder, ExitStatus, PtySize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::convert::TryInto;
use std::io::{Read, Write};
#[cfg(windows)]
use std::os::raw::c_int;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::thread;
use std::time::{Duration, Instant};
use termwiz::escape::csi::{DecPrivateMode, DecPrivateModeCode, Device, Mode};
use termwiz::escape::{Action, CSI};
use thiserror::*;
#[cfg(windows)]
use winapi::um::winsock2::{SOL_SOCKET, SO_RCVBUF, SO_SNDBUF};

pub mod activity;
pub mod client;
pub mod connui;
pub mod domain;
pub mod events;
pub mod layout;
pub mod localpane;
pub mod pane;
pub mod renderable;
pub mod ssh;
pub mod ssh_agent;
pub mod tab;
mod terminfo_renderer;
pub mod termwiztermtab;
pub mod tmux;
pub mod tmux_commands;
mod tmux_pty;
pub mod unify;
pub mod window;

use crate::activity::Activity;

pub const DEFAULT_WORKSPACE: &str = "default";

#[derive(Clone, Debug)]
pub enum MuxNotification {
    PaneOutput(PaneId),
    SynchronizedOutput {
        pane_id: PaneId,
        event: SynchronizedOutputEvent,
    },
    PaneAdded(PaneId),
    PaneRemoved(PaneId),
    WindowCreated(WindowId),
    WindowRemoved(WindowId),
    WindowInvalidated(WindowId),
    WindowWorkspaceChanged(WindowId),
    ActiveWorkspaceChanged(Arc<ClientId>),
    Alert {
        pane_id: PaneId,
        alert: frankenterm_term::Alert,
    },
    Empty,
    AssignClipboard {
        pane_id: PaneId,
        selection: ClipboardSelection,
        clipboard: Option<String>,
    },
    SaveToDownloads {
        name: Option<String>,
        data: Arc<Vec<u8>>,
    },
    TabAddedToWindow {
        tab_id: TabId,
        window_id: WindowId,
    },
    PaneFocused(PaneId),
    TabResized(TabId),
    TabTitleChanged {
        tab_id: TabId,
        title: String,
    },
    WindowTitleChanged {
        window_id: WindowId,
        title: String,
    },
    WorkspaceRenamed {
        old_workspace: String,
        new_workspace: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SynchronizedOutputEvent {
    Depth {
        outcome: SynchronizedOutputDepthOutcome,
        max_depth: u32,
    },
    Admission {
        decision: SynchronizedOutputAdmissionDecision,
        bytes: u64,
    },
    Drain {
        cause: SynchronizedOutputDrainCause,
        bytes: u64,
        depth_outcome: Option<SynchronizedOutputDepthOutcome>,
        max_depth: u32,
    },
    ModeQuery,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SynchronizedOutputDepthOutcome {
    Opened { new_depth: u32 },
    Closed { new_depth: u32 },
    Flushed,
    Underflow,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SynchronizedOutputAdmissionDecision {
    Accepted,
    Truncated { dropped_bytes: u64 },
    Refused,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SynchronizedOutputDrainCause {
    Esu,
    Watchdog,
    LiveResizeForce,
    Operator,
}

static SUB_ID: AtomicUsize = AtomicUsize::new(0);

/// A process-local identifier namespace cannot satisfy a requested reservation.
///
/// `usize::MAX` is an exhausted sentinel, not an identifier that this
/// allocator will ever publish.  Refusing the allocation is essential:
/// saturating at the last value would silently issue the same identifier to
/// multiple live objects.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error(
    "{namespace} identifier space has insufficient remaining capacity for a reservation of {requested} identifier(s); refusing to wrap, saturate, reset, or reuse an identifier"
)]
pub struct IdAllocationError {
    namespace: &'static str,
    requested: usize,
}

impl IdAllocationError {
    pub fn namespace(self) -> &'static str {
        self.namespace
    }

    pub fn requested(self) -> usize {
        self.requested
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("pane identifier {pane_id} is already registered to a different pane instance")]
pub struct PaneIdCollision {
    pub pane_id: PaneId,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error(
    "pane identifier {pane_id} is already being prepared for this pane instance; retry after the in-flight registration completes"
)]
pub struct PanePreparationInProgress {
    pub pane_id: PaneId,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("pane identifier {pane_id} registration was cancelled before publication")]
pub struct PanePreparationCancelled {
    pub pane_id: PaneId,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum DomainRegistrationError {
    #[error("domain identifier {domain_id} ({domain_name}) has been retired and cannot be reused")]
    RetiredIdentifier {
        domain_id: DomainId,
        domain_name: String,
    },
    #[error(
        "domain identifier {domain_id} is already registered to {registered_name}; refusing different instance {requested_name}"
    )]
    IdentifierInUse {
        domain_id: DomainId,
        registered_name: String,
        requested_name: String,
    },
    #[error(
        "domain name {domain_name} is already registered to identifier {registered_id}; refusing identifier {requested_id}"
    )]
    NameInUse {
        domain_name: String,
        registered_id: DomainId,
        requested_id: DomainId,
    },
    #[error("domain registry indexes are inconsistent: {detail}")]
    RegistryInconsistent { detail: String },
    #[error(
        "domain identifier {domain_id} ({domain_name}) is not the exact live registration and cannot become default"
    )]
    DefaultNotRegistered {
        domain_id: DomainId,
        domain_name: String,
    },
}

pub(crate) fn try_reserve_usize_ids(
    counter: &AtomicUsize,
    count: usize,
    namespace: &'static str,
) -> Result<std::ops::Range<usize>, IdAllocationError> {
    // The atomic orders only the uniqueness counter. The locks that publish
    // the resulting objects provide the required visibility ordering, so
    // stronger atomic ordering would add coherence cost without correctness.
    let mut current = counter.load(Ordering::Relaxed);
    if count == 0 {
        return Ok(current..current);
    }

    loop {
        let next = current.checked_add(count).ok_or(IdAllocationError {
            namespace,
            requested: count,
        })?;
        match counter.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return Ok(current..next),
            Err(actual) => current = actual,
        }
    }
}

/// Legacy allocator retained only for the domain, tab, window, and client
/// namespaces pending their fallible-constructor migrations.
///
/// Its terminal-value duplication is deliberately covered as negative
/// evidence below. New identifier namespaces must use
/// `try_reserve_usize_ids`.
pub(crate) fn next_saturating_usize_id(counter: &AtomicUsize) -> usize {
    let mut current = counter.load(Ordering::Relaxed);
    loop {
        let next = current.saturating_add(1);
        match counter.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return current,
            Err(actual) => current = actual,
        }
    }
}

type MuxSubscriber = dyn Fn(MuxNotification) -> bool + Send + Sync;

struct PreparedPaneRegistration {
    pane_id: PaneId,
    reader: Option<Box<dyn std::io::Read + Send>>,
}

struct PanePreparation {
    pane: Weak<dyn Pane>,
    generation: Arc<()>,
}

struct PanePreparationClaim<'a> {
    registration: &'a Mutex<()>,
    claims: &'a Mutex<HashMap<PaneId, PanePreparation>>,
    pane_id: PaneId,
    pane: Weak<dyn Pane>,
    generation: Arc<()>,
    active: bool,
}

impl PanePreparationClaim<'_> {
    /// Return whether this claim still owns the current preparation generation.
    ///
    /// The caller must hold `pane_registration`, which serializes this check
    /// with removal, a subsequent claim, and final publication.
    fn is_authoritative_locked(&self) -> bool {
        self.active
            && self
                .claims
                .lock()
                .get(&self.pane_id)
                .is_some_and(|preparing| {
                    Weak::ptr_eq(&preparing.pane, &self.pane)
                        && Arc::ptr_eq(&preparing.generation, &self.generation)
                })
    }

    /// Retire this exact preparation generation without disturbing a newer
    /// claim for the same pane instance.
    ///
    /// The caller must hold `pane_registration`.
    fn retire_locked(&mut self) -> bool {
        if !self.active {
            return false;
        }
        let mut claims = self.claims.lock();
        let owns_generation = claims.get(&self.pane_id).is_some_and(|preparing| {
            Weak::ptr_eq(&preparing.pane, &self.pane)
                && Arc::ptr_eq(&preparing.generation, &self.generation)
        });
        if owns_generation {
            claims.remove(&self.pane_id);
        }
        self.active = false;
        owns_generation
    }
}

impl Drop for PanePreparationClaim<'_> {
    fn drop(&mut self) {
        if self.active {
            let registration = self.registration;
            let _registration = registration.lock();
            self.retire_locked();
        }
    }
}

/// Releases a successfully spawned pane reader only after publication and its
/// synchronous `PaneAdded` notification are complete.
///
/// Dropping this gate without releasing it closes the channel. The waiting
/// thread then exits without upgrading the pane or invoking the reader.
struct PaneReaderStartGate {
    release_tx: std::sync::mpsc::Sender<(Weak<dyn Pane>, Option<String>, Option<Weak<Mux>>)>,
    pane_id: PaneId,
    pane: Weak<dyn Pane>,
    banner: Option<String>,
}

impl PaneReaderStartGate {
    fn release_if_registered(self, mux: &Mux) {
        let Self {
            release_tx,
            pane_id,
            pane,
            banner,
        } = self;
        let _registration = mux.pane_registration.lock();
        let is_registered = mux
            .panes
            .read()
            .get(&pane_id)
            .is_some_and(|registered| Weak::ptr_eq(&Arc::downgrade(registered), &pane));
        if !is_registered {
            return;
        }
        let owner = Mux::try_get()
            .filter(|installed| std::ptr::eq::<Mux>(installed.as_ref(), mux))
            .map(|installed| Arc::downgrade(&installed));
        if release_tx.send((pane, banner, owner)).is_err() {
            log::error!("spawned pane reader exited before its start gate was released");
        }
    }
}

struct PendingPaneOutputNotification {
    pane_id: PaneId,
    pane: Weak<dyn Pane>,
}

#[derive(Default)]
struct PendingPaneOutputNotifications {
    notifications: Vec<PendingPaneOutputNotification>,
    queued: HashMap<PaneId, Weak<dyn Pane>>,
}

#[derive(Clone, Copy)]
enum PaneLifecycleNotification {
    Added(PaneId),
    Removed(PaneId),
    Output(PaneId),
}

impl From<PaneLifecycleNotification> for MuxNotification {
    fn from(notification: PaneLifecycleNotification) -> Self {
        match notification {
            PaneLifecycleNotification::Added(pane_id) => Self::PaneAdded(pane_id),
            PaneLifecycleNotification::Removed(pane_id) => Self::PaneRemoved(pane_id),
            PaneLifecycleNotification::Output(pane_id) => Self::PaneOutput(pane_id),
        }
    }
}

struct PendingPaneLifecycleNotification {
    notification: PaneLifecycleNotification,
    ready: Arc<AtomicBool>,
    reader_start_gate: Option<PaneReaderStartGate>,
}

#[derive(Default)]
struct PendingPaneLifecycleNotifications {
    notifications: VecDeque<PendingPaneLifecycleNotification>,
    draining: bool,
}

struct PaneLifecycleNotificationTicket {
    ready: Arc<AtomicBool>,
}

struct RemovedPaneRegistration {
    pane_id: PaneId,
    pane: Arc<dyn Pane>,
    lifecycle_notification: PaneLifecycleNotificationTicket,
}

/// Discriminant key for the high-rate Alert variants we dedupe per pane.
///
/// `CurrentWorkingDirectoryChanged` (OSC 7) re-emits on every shell prompt
/// under active agent output; `OutputSinceFocusLost` re-emits on every seqno
/// bump to an unfocused pane. Across N attached muxes these can saturate the
/// notify path with thousands of clones+box allocations per second. Progress
/// is deliberately excluded: Percentage(42) followed by Percentage(64) is a
/// state transition, not a duplicate, and timer-dropping the newer value can
/// leave a remote client stale indefinitely. See ft-18xgy and
/// ft-interactive-systems-performance-4tenz.5.5.1.
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
enum HighRateAlertKind {
    CurrentWorkingDirectoryChanged,
    OutputSinceFocusLost,
}

/// Window during which a `(pane_id, kind)` repeat is dropped at the mux
/// fanout layer. ~1 frame at 60 Hz; below human perception for shell-prompt UI.
const HIGH_RATE_ALERT_DEDUPE_WINDOW: Duration = Duration::from_millis(16);
/// Stale entries older than this are pruned on each insert to keep the dedupe
/// map bounded regardless of pane churn.
const HIGH_RATE_ALERT_PRUNE_AFTER: Duration = Duration::from_secs(1);

pub struct Mux {
    tabs: RwLock<HashMap<TabId, Arc<Tab>>>,
    panes: RwLock<HashMap<PaneId, Arc<dyn Pane>>>,
    pane_preparations: Mutex<HashMap<PaneId, PanePreparation>>,
    pane_registration: Mutex<()>,
    retiring_pane_ids: Mutex<HashSet<PaneId>>,
    pending_pane_lifecycle: Mutex<PendingPaneLifecycleNotifications>,
    windows: RwLock<HashMap<WindowId, Window>>,
    default_domain: RwLock<Option<Arc<dyn Domain>>>,
    domains: RwLock<HashMap<DomainId, Arc<dyn Domain>>>,
    domains_by_name: RwLock<HashMap<String, Arc<dyn Domain>>>,
    domain_registration: Mutex<()>,
    retired_domain_ids: Mutex<HashSet<DomainId>>,
    subscribers: RwLock<HashMap<usize, Arc<MuxSubscriber>>>,
    pending_pane_output: Mutex<PendingPaneOutputNotifications>,
    pane_output_drain_scheduled: AtomicBool,
    banner: RwLock<Option<String>>,
    clients: RwLock<HashMap<ClientId, ClientInfo>>,
    identity: RwLock<Option<Arc<ClientId>>>,
    num_panes_by_workspace: RwLock<HashMap<String, usize>>,
    main_thread_id: std::thread::ThreadId,
    agent: Option<AgentProxy>,
    /// Per-(pane, alert-kind) timestamp of the most recently dispatched
    /// high-rate Alert. Used by `notify` to drop duplicate repeats within
    /// `HIGH_RATE_ALERT_DEDUPE_WINDOW`. ft-18xgy.
    last_high_rate_alert: Mutex<HashMap<(PaneId, HighRateAlertKind), Instant>>,
}

fn mux_socket_buffer_size() -> usize {
    configuration().mux_socket_buffer_size
}

fn max_held_synchronized_output_bytes() -> usize {
    configuration().mux_max_synchronized_output_bytes
}

fn synchronized_output_decrqm_response(hold: bool) -> &'static [u8] {
    if hold {
        b"\x1b[?2026;1$y"
    } else {
        b"\x1b[?2026;2$y"
    }
}

fn respond_to_synchronized_output_query(pane: &Weak<dyn Pane>, hold: bool) {
    let Some(pane) = pane.upgrade() else {
        return;
    };

    let mut writer = pane.writer();
    if let Err(err) = writer.write_all(synchronized_output_decrqm_response(hold)) {
        log::warn!("failed to answer DEC 2026 mode query: {err}");
        return;
    }
    if let Err(err) = writer.flush() {
        log::warn!("failed to flush DEC 2026 mode query response: {err}");
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct SynchronizedOutputActionEffect {
    flush: bool,
    handled: bool,
    depth_outcome: Option<SynchronizedOutputDepthOutcome>,
}

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
struct SynchronizedOutputHold {
    depth: u32,
    max_depth: u32,
}

impl SynchronizedOutputHold {
    fn is_holding(self) -> bool {
        self.depth > 0
    }

    fn max_depth(self) -> u32 {
        self.max_depth
    }

    fn open_bsu(&mut self) -> SynchronizedOutputDepthOutcome {
        self.depth = self.depth.saturating_add(1);
        if self.depth > self.max_depth {
            self.max_depth = self.depth;
        }
        SynchronizedOutputDepthOutcome::Opened {
            new_depth: self.depth,
        }
    }

    fn close_esu(&mut self) -> SynchronizedOutputDepthOutcome {
        if self.depth == 0 {
            return SynchronizedOutputDepthOutcome::Underflow;
        }
        self.depth -= 1;
        if self.depth == 0 {
            SynchronizedOutputDepthOutcome::Flushed
        } else {
            SynchronizedOutputDepthOutcome::Closed {
                new_depth: self.depth,
            }
        }
    }

    fn force_reset(&mut self) -> bool {
        let was_holding = self.is_holding();
        self.depth = 0;
        was_holding
    }
}

fn handle_synchronized_output_action(
    action: &Action,
    hold: &mut SynchronizedOutputHold,
    respond_to_query: impl FnOnce(bool),
) -> SynchronizedOutputActionEffect {
    let mut effect = SynchronizedOutputActionEffect {
        flush: false,
        handled: false,
        depth_outcome: None,
    };

    match action {
        Action::CSI(CSI::Mode(Mode::SetDecPrivateMode(DecPrivateMode::Code(
            DecPrivateModeCode::SynchronizedOutput,
        )))) => {
            effect.depth_outcome = Some(hold.open_bsu());
        }
        Action::CSI(CSI::Mode(Mode::ResetDecPrivateMode(DecPrivateMode::Code(
            DecPrivateModeCode::SynchronizedOutput,
        )))) => {
            let outcome = hold.close_esu();
            effect.flush = matches!(outcome, SynchronizedOutputDepthOutcome::Flushed);
            effect.depth_outcome = Some(outcome);
        }
        Action::CSI(CSI::Device(dev)) if matches!(**dev, Device::SoftReset) => {
            effect.flush = hold.force_reset();
        }
        Action::CSI(CSI::Mode(Mode::QueryDecPrivateMode(DecPrivateMode::Code(
            DecPrivateModeCode::SynchronizedOutput,
        )))) => {
            respond_to_query(hold.is_holding());
            effect.handled = true;
        }
        _ => {}
    }

    effect
}

fn notify_synchronized_output_event(pane: &Weak<dyn Pane>, event: SynchronizedOutputEvent) {
    let Some(pane) = pane.upgrade() else {
        return;
    };
    Mux::notify_from_any_thread(MuxNotification::SynchronizedOutput {
        pane_id: pane.pane_id(),
        event,
    });
}

/// This function applies parsed actions to the pane and notifies any
/// mux subscribers about the output event
fn resolve_pane_reader_mux(owner: &Option<Weak<Mux>>, pane: &Arc<dyn Pane>) -> Option<Arc<Mux>> {
    let mux = match owner {
        Some(owner) => owner.upgrade(),
        None => Mux::try_get(),
    }?;
    mux.get_pane(pane.pane_id())
        .is_some_and(|registered| Arc::ptr_eq(&registered, pane))
        .then_some(mux)
}

fn send_actions_to_mux(
    owner: &Option<Weak<Mux>>,
    pane: &Weak<dyn Pane>,
    dead: &Arc<AtomicBool>,
    actions: Vec<Action>,
) {
    let start = Instant::now();
    match pane.upgrade() {
        Some(pane) => {
            pane.perform_actions(actions);
            histogram!("send_actions_to_mux.perform_actions.latency").record(start.elapsed());
            if let Some(mux) = resolve_pane_reader_mux(owner, &pane) {
                if !mux.enqueue_pane_output_notification_for_pane_with_scheduler_state(
                    &pane,
                    promise::spawn::is_scheduler_configured(),
                ) {
                    dead.store(true, Ordering::Relaxed);
                }
            } else if owner.is_some() {
                // The parser belongs to a pane instance that is no longer
                // registered in its owning mux. Stop the old reader rather
                // than attributing its output to a same-id replacement.
                dead.store(true, Ordering::Relaxed);
            }
        }
        None => {
            // Something else removed the pane from
            // the mux, so signal that we should stop
            // trying to process it in read_from_pane_pty.
            dead.store(true, Ordering::Relaxed);
        }
    }
    histogram!("send_actions_to_mux.rate").record(1.);
}

fn parse_buffered_data(
    owner: Option<Weak<Mux>>,
    pane: Weak<dyn Pane>,
    dead: &Arc<AtomicBool>,
    mut rx: FileDescriptor,
) {
    let mut buf = vec![0; configuration().mux_output_parser_buffer_size];
    let mut parser = termwiz::escape::parser::Parser::new();
    let mut actions = vec![];
    let mut hold = SynchronizedOutputHold::default();
    let mut action_size: usize = 0;
    let mut delay = Duration::from_millis(configuration().mux_output_parser_coalesce_delay_ms);
    let mut deadline = None;

    loop {
        match rx.read(&mut buf) {
            Ok(size) if size == 0 => {
                dead.store(true, Ordering::Relaxed);
                break;
            }
            Err(_) => {
                dead.store(true, Ordering::Relaxed);
                break;
            }
            Ok(size) => {
                let mut chunk_touched_hold = hold.is_holding();
                let mut chunk_admission_emitted = false;
                parser.parse(&buf[0..size], |action| {
                    let was_holding = hold.is_holding();
                    let effect = handle_synchronized_output_action(&action, &mut hold, |hold| {
                        respond_to_synchronized_output_query(&pane, hold);
                    });
                    if was_holding || hold.is_holding() {
                        chunk_touched_hold = true;
                    }
                    if let Some(depth_outcome) = effect.depth_outcome {
                        if effect.flush {
                            if chunk_touched_hold && !chunk_admission_emitted && size > 0 {
                                notify_synchronized_output_event(
                                    &pane,
                                    SynchronizedOutputEvent::Admission {
                                        decision: SynchronizedOutputAdmissionDecision::Accepted,
                                        bytes: size as u64,
                                    },
                                );
                                chunk_admission_emitted = true;
                            }
                            notify_synchronized_output_event(
                                &pane,
                                SynchronizedOutputEvent::Drain {
                                    cause: SynchronizedOutputDrainCause::Esu,
                                    bytes: action_size.saturating_add(size) as u64,
                                    depth_outcome: Some(depth_outcome),
                                    max_depth: hold.max_depth(),
                                },
                            );
                        } else {
                            notify_synchronized_output_event(
                                &pane,
                                SynchronizedOutputEvent::Depth {
                                    outcome: depth_outcome,
                                    max_depth: hold.max_depth(),
                                },
                            );
                        }
                    } else if effect.handled {
                        notify_synchronized_output_event(&pane, SynchronizedOutputEvent::ModeQuery);
                    }
                    if !was_holding && hold.is_holding() && !actions.is_empty() {
                        // Flush prior actions before entering BSU hold.
                        send_actions_to_mux(&owner, &pane, &dead, std::mem::take(&mut actions));
                        action_size = 0;
                    }
                    if !effect.handled {
                        action.append_to(&mut actions);
                    }

                    if effect.flush && !actions.is_empty() {
                        send_actions_to_mux(&owner, &pane, &dead, std::mem::take(&mut actions));
                        action_size = 0;
                    }
                });
                if chunk_touched_hold && !chunk_admission_emitted && size > 0 {
                    notify_synchronized_output_event(
                        &pane,
                        SynchronizedOutputEvent::Admission {
                            decision: SynchronizedOutputAdmissionDecision::Accepted,
                            bytes: size as u64,
                        },
                    );
                }
                action_size += size;
                if hold.is_holding() && action_size >= max_held_synchronized_output_bytes() {
                    // A buggy app can enter synchronized-output mode and never
                    // send the reset sequence. Bound buffered memory in that case.
                    log::warn!(
                        "forcing synchronized-output flush after {} buffered bytes without reset",
                        action_size
                    );
                    hold.force_reset();
                    notify_synchronized_output_event(
                        &pane,
                        SynchronizedOutputEvent::Drain {
                            cause: SynchronizedOutputDrainCause::Watchdog,
                            bytes: action_size as u64,
                            depth_outcome: None,
                            max_depth: hold.max_depth(),
                        },
                    );
                    if !actions.is_empty() {
                        send_actions_to_mux(&owner, &pane, &dead, std::mem::take(&mut actions));
                    }
                    deadline = None;
                    action_size = 0;
                }
                if !actions.is_empty() && !hold.is_holding() {
                    // If we haven't accumulated too much data,
                    // pause for a short while to increase the chances
                    // that we coalesce a full "frame" from an unoptimized
                    // TUI program
                    if action_size < buf.len() {
                        let poll_delay = match deadline {
                            None => {
                                if let Some(target) = Instant::now().checked_add(delay) {
                                    deadline.replace(target);
                                    Some(delay)
                                } else {
                                    log::warn!(
                                        "mux output parser coalesce delay is too large for Instant; flushing without delay"
                                    );
                                    None
                                }
                            }
                            Some(target) => target.checked_duration_since(Instant::now()),
                        };
                        if poll_delay.is_some() {
                            let mut pfd = [pollfd {
                                fd: rx.as_socket_descriptor(),
                                events: POLLIN,
                                revents: 0,
                            }];
                            if let Ok(1) = poll(&mut pfd, poll_delay) {
                                // We can read now without blocking, so accumulate
                                // more data into actions
                                continue;
                            }

                            // Not readable in time: let the data we have flow into
                            // the terminal model
                        }
                    }

                    send_actions_to_mux(&owner, &pane, &dead, std::mem::take(&mut actions));
                    deadline = None;
                    action_size = 0;
                }

                let config = configuration();
                buf.resize(config.mux_output_parser_buffer_size, 0);
                delay = Duration::from_millis(config.mux_output_parser_coalesce_delay_ms);
            }
        }
    }

    // Don't forget to send anything that we might have buffered
    // to be displayed before we return from here; this is important
    // for very short lived commands so that we don't forget to
    // display what they displayed.
    if !actions.is_empty() {
        send_actions_to_mux(&owner, &pane, &dead, std::mem::take(&mut actions));
    }
}

fn set_socket_buffer(fd: &mut FileDescriptor, option: i32, size: usize) -> anyhow::Result<()> {
    let size = size as c_int;
    let socklen = std::mem::size_of_val(&size);
    unsafe {
        let res = libc::setsockopt(
            fd.as_socket_descriptor(),
            SOL_SOCKET,
            option,
            &size as *const c_int as *const _,
            socklen as _,
        );
        if res == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error()).context("setsockopt")
        }
    }
}

fn allocate_socketpair() -> anyhow::Result<(FileDescriptor, FileDescriptor)> {
    let (mut tx, mut rx) = socketpair().context("socketpair")?;
    set_socket_buffer(&mut tx, SO_SNDBUF, mux_socket_buffer_size())
        .context("SO_SNDBUF")
        .ok();
    set_socket_buffer(&mut rx, SO_RCVBUF, mux_socket_buffer_size())
        .context("SO_RCVBUF")
        .ok();
    Ok((tx, rx))
}

/// This function is run in a separate thread; its purpose is to perform
/// blocking reads from the pty (non-blocking reads are not portable to
/// all platforms and pty/tty types), parse the escape sequences and
/// relay the actions to the mux thread to apply them to the pane.
fn read_from_pane_pty(
    owner: Option<Weak<Mux>>,
    pane: Weak<dyn Pane>,
    banner: Option<String>,
    mut reader: Box<dyn std::io::Read>,
) {
    let mut buf = vec![0; mux_socket_buffer_size()];

    // This is used to signal that an error occurred either in this thread,
    // or in the main mux thread.  If `true`, this thread will terminate.
    let dead = Arc::new(AtomicBool::new(false));

    let pane_for_lifecycle = Weak::clone(&pane);
    let (pane_id, exit_behavior) = match pane.upgrade() {
        Some(pane) => (pane.pane_id(), pane.exit_behavior()),
        None => return,
    };

    let (mut tx, rx) = match allocate_socketpair() {
        Ok(pair) => pair,
        Err(err) => {
            log::error!("read_from_pane_pty: Unable to allocate a socketpair: {err:#}");
            localpane::emit_output_for_pane(
                pane_id,
                &format!(
                    "⚠️  FrankenTerm: read_from_pane_pty: \
                    Unable to allocate a socketpair: {err:#}"
                ),
            );
            return;
        }
    };

    if let Err(err) = std::thread::Builder::new()
        .name(format!("mux-parse-pane-{pane_id}"))
        .spawn({
            let dead = Arc::clone(&dead);
            let parser_owner = owner.clone();
            move || parse_buffered_data(parser_owner, pane, &dead, rx)
        })
    {
        log::error!("read_from_pane_pty: Unable to spawn parser thread: {err:#}");
        localpane::emit_output_for_pane(
            pane_id,
            &format!("FrankenTerm: read_from_pane_pty: Unable to spawn parser thread: {err:#}"),
        );
        return;
    }

    if let Some(banner) = banner {
        tx.write_all(banner.as_bytes()).ok();
    }

    while !dead.load(Ordering::Relaxed) {
        match reader.read(&mut buf) {
            Ok(size) if size == 0 => {
                log::trace!("read_pty EOF: pane_id {}", pane_id);
                break;
            }
            Err(err) => {
                error!("read_pty failed: pane {} {:?}", pane_id, err);
                break;
            }
            Ok(size) => {
                histogram!("read_from_pane_pty.bytes.rate").record(size as f64);
                log::trace!("read_pty pane {pane_id} read {size} bytes");
                if let Err(err) = tx.write_all(&buf[..size]) {
                    error!(
                        "read_pty failed to write to parser: pane {} {:?}",
                        pane_id, err
                    );
                    break;
                }
            }
        }
    }

    match exit_behavior.unwrap_or_else(|| configuration().exit_behavior) {
        ExitBehavior::Hold | ExitBehavior::CloseOnCleanExit => {
            // We don't know if we can unilaterally close
            // this pane right now, so don't!
            if promise::spawn::is_scheduler_configured() {
                promise::spawn::spawn_into_main_thread(async move {
                    if let Some(expected) = pane_for_lifecycle.upgrade() {
                        if let Some(mux) = resolve_pane_reader_mux(&owner, &expected) {
                            log::trace!("checking for dead windows after EOF on pane {}", pane_id);
                            mux.prune_dead_windows();
                        }
                    }
                })
                .detach();
            }
        }
        ExitBehavior::Close => {
            if promise::spawn::is_scheduler_configured() {
                promise::spawn::spawn_into_main_thread(async move {
                    if let Some(expected) = pane_for_lifecycle.upgrade() {
                        if let Some(mux) = resolve_pane_reader_mux(&owner, &expected) {
                            mux.remove_pane_if_same(pane_id, &expected);
                            mux.prune_dead_windows();
                        }
                    }
                })
                .detach();
            }
        }
    }

    dead.store(true, Ordering::Relaxed);
}

lazy_static::lazy_static! {
    static ref MUX: Mutex<Option<Arc<Mux>>> = Mutex::new(None);
}

#[cfg(test)]
pub(crate) static MUX_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub struct MuxWindowBuilder {
    window_id: WindowId,
    activity: Option<Activity>,
    notified: bool,
}

impl MuxWindowBuilder {
    /// Releases a provisional window without publishing `WindowCreated`.
    ///
    /// Tmux control-mode domains retain a builder while their remote window
    /// topology materializes. Terminal cleanup must be able to release that
    /// activity lease without making an abandoned window visible.
    pub(crate) fn cancel(mut self) {
        self.notified = true;
        let _ = self.activity.take();
    }

    fn notify(&mut self) {
        if self.notified {
            return;
        }
        self.notified = true;
        let Some(activity) = self.activity.take() else {
            return;
        };
        let window_id = self.window_id;
        let Some(mux) = Mux::try_get() else {
            return;
        };
        if mux.is_main_thread() {
            // If we're already on the mux thread, just send the notification
            // immediately.
            // This is super important for Wayland; if we push it to the
            // spawn queue below then the extra milliseconds of delay
            // causes it to get confused and shutdown the connection!?
            mux.notify(MuxNotification::WindowCreated(window_id));
        } else if promise::spawn::is_scheduler_configured() {
            promise::spawn::spawn_into_main_thread(async move {
                if let Some(mux) = Mux::try_get() {
                    mux.notify(MuxNotification::WindowCreated(window_id));
                    drop(activity);
                }
            })
            .detach();
        } else {
            mux.notify(MuxNotification::WindowCreated(window_id));
        }
    }
}

impl Drop for MuxWindowBuilder {
    fn drop(&mut self) {
        self.notify();
    }
}

impl std::ops::Deref for MuxWindowBuilder {
    type Target = WindowId;

    fn deref(&self) -> &WindowId {
        &self.window_id
    }
}

impl Mux {
    pub fn new(default_domain: Option<Arc<dyn Domain>>) -> Self {
        let mut domains = HashMap::new();
        let mut domains_by_name = HashMap::new();
        if let Some(default_domain) = default_domain.as_ref() {
            domains.insert(default_domain.domain_id(), Arc::clone(default_domain));

            domains_by_name.insert(
                default_domain.domain_name().to_string(),
                Arc::clone(default_domain),
            );
        }

        let agent = if config::configuration().mux_enable_ssh_agent {
            Some(AgentProxy::new())
        } else {
            None
        };

        Self {
            tabs: RwLock::new(HashMap::new()),
            panes: RwLock::new(HashMap::new()),
            pane_preparations: Mutex::new(HashMap::new()),
            pane_registration: Mutex::new(()),
            retiring_pane_ids: Mutex::new(HashSet::new()),
            pending_pane_lifecycle: Mutex::new(PendingPaneLifecycleNotifications::default()),
            windows: RwLock::new(HashMap::new()),
            default_domain: RwLock::new(default_domain),
            domains_by_name: RwLock::new(domains_by_name),
            domains: RwLock::new(domains),
            domain_registration: Mutex::new(()),
            retired_domain_ids: Mutex::new(HashSet::new()),
            subscribers: RwLock::new(HashMap::new()),
            pending_pane_output: Mutex::new(PendingPaneOutputNotifications::default()),
            pane_output_drain_scheduled: AtomicBool::new(false),
            banner: RwLock::new(None),
            clients: RwLock::new(HashMap::new()),
            identity: RwLock::new(None),
            num_panes_by_workspace: RwLock::new(HashMap::new()),
            main_thread_id: std::thread::current().id(),
            agent,
            last_high_rate_alert: Mutex::new(HashMap::new()),
        }
    }

    fn get_default_workspace(&self) -> String {
        let config = configuration();
        config
            .default_workspace
            .as_deref()
            .unwrap_or(DEFAULT_WORKSPACE)
            .to_string()
    }

    pub fn is_main_thread(&self) -> bool {
        std::thread::current().id() == self.main_thread_id
    }

    fn recompute_pane_count(&self) {
        let mut count = HashMap::new();
        for window in self.windows.read().values() {
            let workspace = window.get_workspace();
            for tab in window.iter() {
                *count.entry(workspace.to_string()).or_insert(0) += match tab.count_panes() {
                    Some(n) => n,
                    None => {
                        // Busy: abort this and we'll retry later
                        return;
                    }
                };
            }
        }
        *self.num_panes_by_workspace.write() = count;
    }

    pub fn client_had_input(&self, client_id: &ClientId) {
        if let Some(info) = self.clients.write().get_mut(client_id) {
            info.update_last_input();
        }
        if let Some(agent) = &self.agent {
            agent.update_target();
        }
    }

    pub fn record_input_for_current_identity(&self) {
        if let Some(ident) = self.identity.read().as_ref() {
            self.client_had_input(ident);
        }
    }

    pub fn record_focus_for_current_identity(&self, pane_id: PaneId) {
        if let Some(ident) = self.identity.read().as_ref() {
            self.record_focus_for_client(ident, pane_id);
        }
    }

    pub fn resolve_focused_pane(
        &self,
        client_id: &ClientId,
    ) -> Option<(DomainId, WindowId, TabId, PaneId)> {
        let pane_id = self.clients.read().get(client_id)?.focused_pane_id?;
        let (domain, window, tab) = self.resolve_pane_id(pane_id)?;
        Some((domain, window, tab, pane_id))
    }

    pub fn record_focus_for_client(&self, client_id: &ClientId, pane_id: PaneId) {
        let mut prior = None;
        if let Some(info) = self.clients.write().get_mut(client_id) {
            prior = info.focused_pane_id;
            info.update_focused_pane(pane_id);
        }

        if prior == Some(pane_id) {
            return;
        }
        // Synthesize focus events
        if let Some(prior_id) = prior {
            if let Some(pane) = self.get_pane(prior_id) {
                pane.focus_changed(false);
            }
        }
        if let Some(pane) = self.get_pane(pane_id) {
            pane.focus_changed(true);
        }
    }

    /// Called by PaneFocused event handlers to reconcile a remote
    /// pane focus event and apply its effects locally
    pub fn focus_pane_and_containing_tab(&self, pane_id: PaneId) -> anyhow::Result<()> {
        let pane = self
            .get_pane(pane_id)
            .ok_or_else(|| anyhow::anyhow!("pane {pane_id} not found"))?;

        let (_domain, window_id, tab_id) = self
            .resolve_pane_id(pane_id)
            .ok_or_else(|| anyhow::anyhow!("can't find {pane_id} in the mux"))?;

        // Focus/activate the containing tab within its window
        {
            let mut win = self
                .get_window_mut(window_id)
                .ok_or_else(|| anyhow::anyhow!("window_id {window_id} not found"))?;
            let tab_idx = win
                .idx_by_id(tab_id)
                .ok_or_else(|| anyhow::anyhow!("tab {tab_id} not in {window_id}"))?;
            win.save_and_then_set_active(tab_idx);
        }

        // Focus/activate the pane locally
        let tab = self
            .get_tab(tab_id)
            .ok_or_else(|| anyhow::anyhow!("tab {tab_id} not found"))?;

        tab.set_active_pane(&pane);

        Ok(())
    }

    pub fn register_client(&self, client_id: Arc<ClientId>) {
        self.clients
            .write()
            .insert((*client_id).clone(), ClientInfo::new(client_id));
    }

    pub fn iter_clients(&self) -> Vec<ClientInfo> {
        self.clients
            .read()
            .values()
            .map(|info| info.clone())
            .collect()
    }

    /// Returns a list of the unique workspace names known to the mux.
    /// This is taken from all known windows.
    pub fn iter_workspaces(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .windows
            .read()
            .values()
            .map(|w| w.get_workspace().to_string())
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// Generate a new unique workspace name
    pub fn generate_workspace_name(&self) -> String {
        let used = self.iter_workspaces();
        for candidate in names::Generator::default() {
            if !used.contains(&candidate) {
                return candidate;
            }
        }
        unreachable!();
    }

    /// Returns the effective active workspace name
    pub fn active_workspace(&self) -> String {
        self.identity
            .read()
            .as_ref()
            .and_then(|ident| {
                self.clients
                    .read()
                    .get(&ident)
                    .and_then(|info| info.active_workspace.clone())
            })
            .unwrap_or_else(|| self.get_default_workspace())
    }

    /// Returns the effective active workspace name for a given client
    pub fn active_workspace_for_client(&self, ident: &Arc<ClientId>) -> String {
        self.clients
            .read()
            .get(&ident)
            .and_then(|info| info.active_workspace.clone())
            .unwrap_or_else(|| self.get_default_workspace())
    }

    pub fn set_active_workspace_for_client(&self, ident: &Arc<ClientId>, workspace: &str) {
        let changed = {
            let mut clients = self.clients.write();
            clients.get_mut(&ident).is_some_and(|info| {
                info.active_workspace.replace(workspace.to_string());
                true
            })
        };
        if changed {
            self.notify(MuxNotification::ActiveWorkspaceChanged(ident.clone()));
        }
    }

    /// Assigns the active workspace name for the current identity
    pub fn set_active_workspace(&self, workspace: &str) {
        if let Some(ident) = self.identity.read().clone() {
            self.set_active_workspace_for_client(&ident, workspace);
        }
    }

    pub fn rename_workspace(&self, old_workspace: &str, new_workspace: &str) {
        if old_workspace == new_workspace {
            return;
        }
        self.notify(MuxNotification::WorkspaceRenamed {
            old_workspace: old_workspace.to_string(),
            new_workspace: new_workspace.to_string(),
        });

        for window in self.windows.write().values_mut() {
            if window.get_workspace() == old_workspace {
                window.set_workspace(new_workspace);
            }
        }
        self.recompute_pane_count();
        let changed_clients = {
            let mut clients = self.clients.write();
            clients
                .values_mut()
                .filter_map(|client| {
                    if client.active_workspace.as_deref() == Some(old_workspace) {
                        client.active_workspace.replace(new_workspace.to_string());
                        Some(Arc::clone(&client.client_id))
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
        };
        for client_id in changed_clients {
            self.notify(MuxNotification::ActiveWorkspaceChanged(client_id));
        }
    }

    /// Overrides the current client identity.
    /// Returns `IdentityHolder` which will restore the prior identity
    /// when it is dropped.
    /// This can be used to change the identity for the duration of a block.
    pub fn with_identity(&self, id: Option<Arc<ClientId>>) -> IdentityHolder {
        let prior = self.replace_identity(id);
        IdentityHolder { prior }
    }

    /// Replace the identity, returning the prior identity
    pub fn replace_identity(&self, id: Option<Arc<ClientId>>) -> Option<Arc<ClientId>> {
        std::mem::replace(&mut *self.identity.write(), id)
    }

    /// Returns the active identity
    pub fn active_identity(&self) -> Option<Arc<ClientId>> {
        self.identity.read().clone()
    }

    pub fn unregister_client(&self, client_id: &ClientId) {
        self.clients.write().remove(client_id);
        let mut identity = self.identity.write();
        if identity
            .as_ref()
            .is_some_and(|ident| ident.as_ref() == client_id)
        {
            *identity = None;
        }
    }

    pub fn unregister_client_if_same(&self, client_id: &Arc<ClientId>) -> bool {
        let removed = {
            let mut clients = self.clients.write();
            let owns_registration = clients
                .get(client_id.as_ref())
                .is_some_and(|info| Arc::ptr_eq(&info.client_id, client_id));
            if owns_registration {
                clients.remove(client_id.as_ref());
                true
            } else {
                false
            }
        };
        if !removed {
            return false;
        }

        let mut identity = self.identity.write();
        if identity
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, client_id))
        {
            *identity = None;
        }
        true
    }

    pub fn subscribe<F>(&self, subscriber: F) -> Result<usize, IdAllocationError>
    where
        F: Fn(MuxNotification) -> bool + 'static + Send + Sync,
    {
        let sub_id = try_reserve_usize_ids(&SUB_ID, 1, "mux subscriber")?.start;
        self.subscribers
            .write()
            .insert(sub_id, Arc::new(subscriber));
        Ok(sub_id)
    }

    pub fn unsubscribe(&self, sub_id: usize) -> bool {
        self.subscribers.write().remove(&sub_id).is_some()
    }

    pub fn notify(&self, notification: MuxNotification) {
        // Dedupe high-rate Alert variants per (pane, kind) within
        // HIGH_RATE_ALERT_DEDUPE_WINDOW. Saves N_subscribers × clone +
        // box-allocation per dropped notification under bursty agent output.
        // See ft-18xgy.
        if let MuxNotification::Alert { pane_id, alert } = &notification {
            let kind = match alert {
                frankenterm_term::Alert::CurrentWorkingDirectoryChanged => {
                    Some(HighRateAlertKind::CurrentWorkingDirectoryChanged)
                }
                frankenterm_term::Alert::OutputSinceFocusLost => {
                    Some(HighRateAlertKind::OutputSinceFocusLost)
                }
                _ => None,
            };
            if let Some(kind) = kind {
                let now = Instant::now();
                let key = (*pane_id, kind);
                let mut last = self.last_high_rate_alert.lock();
                // Dedup check first so the deduped path doesn't pay the
                // O(map_size) prune cost. `saturating_duration_since` keeps
                // the comparison safe under non-monotonic clock anomalies
                // (rare, but Instant on Windows isn't strictly monotonic).
                if let Some(prev) = last.get(&key) {
                    if now.saturating_duration_since(*prev) < HIGH_RATE_ALERT_DEDUPE_WINDOW {
                        histogram!("mux.notifications.high_rate_alert.deduped").record(1.);
                        return;
                    }
                }
                // Only on the insert path: best-effort prune of stale entries.
                // With <100 panes per host the map stays trivially small.
                last.retain(|_, ts| {
                    now.saturating_duration_since(*ts) < HIGH_RATE_ALERT_PRUNE_AFTER
                });
                last.insert(key, now);
            }
        }

        match notification {
            MuxNotification::PaneOutput(pane_id) => self.enqueue_pane_output_notification(pane_id),
            notification => self.dispatch_notification(notification),
        }
    }

    pub fn notify_from_any_thread(notification: MuxNotification) {
        if let MuxNotification::PaneOutput(pane_id) = notification {
            if let Some(mux) = Mux::try_get() {
                mux.enqueue_pane_output_notification(pane_id);
            }
            return;
        }
        if let Some(mux) = Mux::try_get() {
            if mux.is_main_thread() {
                mux.notify(notification);
                return;
            }
        }
        if promise::spawn::is_scheduler_configured() {
            promise::spawn::spawn_into_main_thread(async {
                if let Some(mux) = Mux::try_get() {
                    mux.notify(notification);
                }
            })
            .detach();
        }
    }

    // Callbacks are invoked without holding the subscribers lock. A callback
    // removed concurrently may still observe the current notification if it
    // was present in the snapshot; removals only affect future notifications.
    fn dispatch_notification(&self, notification: MuxNotification) {
        let subscribers = self
            .subscribers
            .read()
            .iter()
            .map(|(id, subscriber)| (*id, Arc::clone(subscriber)))
            .collect::<Vec<_>>();
        histogram!("mux.notifications.subscriber_fanout").record(subscribers.len() as f64);

        let mut dead_subscribers = Vec::new();
        for (id, subscriber) in subscribers {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                subscriber(notification.clone())
            })) {
                Ok(true) => {} // subscriber still alive
                Ok(false) => dead_subscribers.push(id),
                Err(_) => {
                    log::error!("mux subscriber {id} panicked — removing");
                    dead_subscribers.push(id);
                }
            }
        }

        if !dead_subscribers.is_empty() {
            let mut subscribers = self.subscribers.write();
            for id in dead_subscribers {
                subscribers.remove(&id);
            }
        }
    }

    /// Enqueue a pane lifecycle transition at the same linearization point as
    /// its topology mutation. The caller must hold `pane_registration`.
    ///
    /// The returned ticket keeps the event undispatchable until any required
    /// outside-lock work (notably `Pane::kill`) has completed.
    fn enqueue_pane_lifecycle_notification_locked(
        &self,
        notification: PaneLifecycleNotification,
        reader_start_gate: Option<PaneReaderStartGate>,
    ) -> PaneLifecycleNotificationTicket {
        let ready = Arc::new(AtomicBool::new(false));
        self.pending_pane_lifecycle.lock().notifications.push_back(
            PendingPaneLifecycleNotification {
                notification,
                ready: Arc::clone(&ready),
                reader_start_gate,
            },
        );
        PaneLifecycleNotificationTicket { ready }
    }

    /// Mark one queued lifecycle transition ready and arrange for exactly one
    /// caller to drain all ready transitions in topology-mutation order.
    ///
    /// This method must be called without `pane_registration` or a subscriber
    /// lock held; `dispatch_notification` snapshots subscribers before invoking
    /// any callback.
    fn complete_pane_lifecycle_notification(&self, ticket: PaneLifecycleNotificationTicket) {
        ticket.ready.store(true, Ordering::Release);
        let should_drain = {
            let mut pending = self.pending_pane_lifecycle.lock();
            if pending.draining {
                false
            } else if pending
                .notifications
                .front()
                .is_some_and(|notification| notification.ready.load(Ordering::Acquire))
            {
                pending.draining = true;
                true
            } else {
                false
            }
        };
        if should_drain {
            self.drain_pane_lifecycle_notifications();
        }
    }

    fn drain_pane_lifecycle_notifications(&self) {
        loop {
            let pending_notification = {
                let mut pending = self.pending_pane_lifecycle.lock();
                match pending.notifications.front() {
                    Some(notification) if notification.ready.load(Ordering::Acquire) => {
                        pending.notifications.pop_front()
                    }
                    Some(_) | None => {
                        pending.draining = false;
                        None
                    }
                }
            };
            let Some(pending_notification) = pending_notification else {
                return;
            };
            let notification = pending_notification.notification;
            self.dispatch_notification(notification.into());
            if let Some(reader_start_gate) = pending_notification.reader_start_gate {
                reader_start_gate.release_if_registered(self);
            }
            if let PaneLifecycleNotification::Removed(pane_id) = notification {
                // The removal queue entry owns the retirement fence. A caller
                // may complete a ticket reentrantly while an earlier callback
                // is draining; clearing in the caller would then permit a
                // same-ID replacement before this callback actually ran.
                let _registration = self.pane_registration.lock();
                self.retiring_pane_ids.lock().remove(&pane_id);
            }
        }
    }

    fn enqueue_pane_output_notification(&self, pane_id: PaneId) {
        let Some(pane) = self.get_pane(pane_id) else {
            return;
        };
        let _ = self.enqueue_pane_output_notification_for_pane_with_scheduler_state(
            &pane,
            promise::spawn::is_scheduler_configured(),
        );
    }

    fn enqueue_pane_output_notification_for_pane_with_scheduler_state(
        &self,
        pane: &Arc<dyn Pane>,
        scheduler_configured: bool,
    ) -> bool {
        let pane_id = pane.pane_id();
        let should_schedule = {
            let _registration = self.pane_registration.lock();
            let registered = self
                .panes
                .read()
                .get(&pane_id)
                .is_some_and(|current| Arc::ptr_eq(current, pane));
            if !registered {
                return false;
            }

            let pane = Arc::downgrade(pane);
            let mut pending = self.pending_pane_output.lock();
            let already_queued = pending
                .queued
                .get(&pane_id)
                .is_some_and(|queued| Weak::ptr_eq(queued, &pane));
            if !already_queued {
                pending.queued.insert(pane_id, pane.clone());
                pending
                    .notifications
                    .push(PendingPaneOutputNotification { pane_id, pane });
                histogram!("mux.notifications.pane_output.unique_enqueue_rate").record(1.);
            }
            !self
                .pane_output_drain_scheduled
                .swap(true, Ordering::AcqRel)
        };

        if !should_schedule {
            return true;
        }

        let exact_mux = scheduler_configured
            .then(Mux::try_get)
            .flatten()
            .filter(|mux| std::ptr::eq::<Mux>(mux.as_ref(), self));
        if let Some(exact_mux) = exact_mux {
            let weak_mux = Arc::downgrade(&exact_mux);
            promise::spawn::spawn_into_main_thread(async move {
                if let Some(mux) = weak_mux.upgrade() {
                    mux.flush_pending_pane_output_notifications();
                }
            })
            .detach();
        } else {
            // Standalone/headless embedders may intentionally construct a mux
            // without configuring the GUI scheduler or without installing it
            // as the global mux. Preserve the historical direct-notify
            // contract by draining synchronously in either case.
            self.flush_pending_pane_output_notifications();
        }
        true
    }

    #[cfg(test)]
    fn discard_pending_pane_output_notification(&self, pane_id: PaneId) {
        let mut pending = self.pending_pane_output.lock();
        if pending.queued.remove(&pane_id).is_some() {
            pending
                .notifications
                .retain(|notification| notification.pane_id != pane_id);
        }
    }

    fn discard_removed_pane_states(&self, pane_ids: &[PaneId]) {
        if pane_ids.is_empty() {
            return;
        }
        let pane_ids = pane_ids.iter().copied().collect::<HashSet<_>>();
        self.last_high_rate_alert
            .lock()
            .retain(|(pane_id, _), _| !pane_ids.contains(pane_id));
        for client in self.clients.write().values_mut() {
            if client
                .focused_pane_id
                .is_some_and(|pane_id| pane_ids.contains(&pane_id))
            {
                client.focused_pane_id = None;
            }
        }
        let mut pending = self.pending_pane_output.lock();
        pending
            .queued
            .retain(|pane_id, _| !pane_ids.contains(pane_id));
        pending
            .notifications
            .retain(|notification| !pane_ids.contains(&notification.pane_id));
    }

    fn flush_pending_pane_output_notifications(&self) {
        loop {
            let notifications = {
                let mut pending = self.pending_pane_output.lock();
                if pending.notifications.is_empty() {
                    self.pane_output_drain_scheduled
                        .store(false, Ordering::Release);
                    return;
                }
                std::mem::take(&mut pending.notifications)
            };

            histogram!("mux.notifications.pane_output.batch_size")
                .record(notifications.len() as f64);
            for notification in notifications {
                let ticket = {
                    let _registration = self.pane_registration.lock();
                    let mut pending = self.pending_pane_output.lock();
                    let is_latest = pending
                        .queued
                        .get(&notification.pane_id)
                        .is_some_and(|queued| Weak::ptr_eq(queued, &notification.pane));
                    if !is_latest {
                        None
                    } else {
                        pending.queued.remove(&notification.pane_id);
                        let is_registered = notification.pane.upgrade().is_some_and(|pane| {
                            self.panes
                                .read()
                                .get(&notification.pane_id)
                                .is_some_and(|registered| Arc::ptr_eq(registered, &pane))
                        });
                        is_registered.then(|| {
                            self.enqueue_pane_lifecycle_notification_locked(
                                PaneLifecycleNotification::Output(notification.pane_id),
                                None,
                            )
                        })
                    }
                };
                if let Some(ticket) = ticket {
                    // Output and lifecycle transitions share one ordered
                    // publication stream. If removal linearized first, the
                    // exact registration check above drops this output; if
                    // output linearized first, subscribers observe it before
                    // the corresponding PaneRemoved transition.
                    self.complete_pane_lifecycle_notification(ticket);
                }
            }
        }
    }

    pub fn default_domain(&self) -> Arc<dyn Domain> {
        self.default_domain.read().as_ref().map(Arc::clone).unwrap()
    }

    fn resolve_default_domain(&self) -> anyhow::Result<Arc<dyn Domain>> {
        self.default_domain
            .read()
            .as_ref()
            .map(Arc::clone)
            .ok_or_else(|| anyhow!("no default domain configured"))
    }

    pub fn set_default_domain(
        &self,
        domain: &Arc<dyn Domain>,
    ) -> Result<(), DomainRegistrationError> {
        let domain_id = domain.domain_id();
        let domain_name = domain.domain_name().to_string();
        let _registration = self.domain_registration.lock();
        let exact_id = self
            .domains
            .read()
            .get(&domain_id)
            .is_some_and(|registered| Arc::ptr_eq(registered, domain));
        let exact_name = self
            .domains_by_name
            .read()
            .get(&domain_name)
            .is_some_and(|registered| Arc::ptr_eq(registered, domain));
        if self.retired_domain_ids.lock().contains(&domain_id) || !exact_id || !exact_name {
            return Err(DomainRegistrationError::DefaultNotRegistered {
                domain_id,
                domain_name,
            });
        }
        *self.default_domain.write() = Some(Arc::clone(domain));
        Ok(())
    }

    pub fn get_domain(&self, id: DomainId) -> Option<Arc<dyn Domain>> {
        self.domains.read().get(&id).cloned()
    }

    pub fn get_domain_by_name(&self, name: &str) -> Option<Arc<dyn Domain>> {
        self.domains_by_name.read().get(name).cloned()
    }

    pub fn add_domain(&self, domain: &Arc<dyn Domain>) -> Result<(), DomainRegistrationError> {
        // Domain implementations are external callbacks. Resolve requested
        // metadata before taking registry locks so a reentrant or blocking
        // implementation cannot freeze every domain reader/writer.
        let domain_id = domain.domain_id();
        let domain_name = domain.domain_name().to_string();
        let domain_arc = Arc::clone(domain);
        let _registration = self.domain_registration.lock();

        if self.retired_domain_ids.lock().contains(&domain_id) {
            return Err(DomainRegistrationError::RetiredIdentifier {
                domain_id,
                domain_name,
            });
        }

        {
            let mut domains = self.domains.write();
            let mut domains_by_name = self.domains_by_name.write();
            if let Some(existing) = domains.get(&domain_id) {
                if Arc::ptr_eq(existing, domain) {
                    return Ok(());
                }
                let registered_name = domains_by_name
                    .iter()
                    .find_map(|(name, registered)| {
                        Arc::ptr_eq(registered, existing).then(|| name.clone())
                    })
                    .ok_or_else(|| DomainRegistrationError::RegistryInconsistent {
                        detail: format!(
                            "identifier {domain_id} has no exact name-index registration"
                        ),
                    })?;
                return Err(DomainRegistrationError::IdentifierInUse {
                    domain_id,
                    registered_name,
                    requested_name: domain_name,
                });
            }
            if let Some(existing) = domains_by_name.get(&domain_name) {
                if !Arc::ptr_eq(existing, domain) {
                    let registered_id = domains
                        .iter()
                        .find_map(|(id, registered)| {
                            Arc::ptr_eq(registered, existing).then_some(*id)
                        })
                        .ok_or_else(|| DomainRegistrationError::RegistryInconsistent {
                            detail: format!(
                                "name {domain_name} has no exact identifier-index registration"
                            ),
                        })?;
                    return Err(DomainRegistrationError::NameInUse {
                        domain_name,
                        registered_id,
                        requested_id: domain_id,
                    });
                }
                return Ok(());
            }
            domains_by_name.insert(domain_name, Arc::clone(&domain_arc));
            domains.insert(domain_id, Arc::clone(&domain_arc));
        }

        let mut default_domain = self.default_domain.write();
        if default_domain.is_none() {
            *default_domain = Some(domain_arc);
        }
        Ok(())
    }

    pub fn set_mux(mux: &Arc<Mux>) {
        // Drop the replaced mux only after releasing the singleton lock. The
        // old mux's last Arc may run ClientDomain teardown, which calls back
        // into `Mux::try_get`.
        let replaced = MUX.lock().replace(Arc::clone(mux));
        drop(replaced);
    }

    pub fn shutdown() {
        // Important: bind the taken Arc<Mux> to a `let` so the MutexGuard
        // returned by MUX.lock() is dropped at the end of the *statement*
        // (i.e., right here), BEFORE `taken` itself is dropped at end of
        // function. Without the let-binding, a temporary-drop-order
        // deadlock fires:
        //
        //   MUX.lock().take();           // as one statement, temporaries
        //                                // dropped reverse-of-construction
        //   ── drops Option<Arc<Mux>> first  ⇨ Mux::drop ⇨ ClientDomain::drop
        //         which calls Mux::try_get  ⇨ tries to acquire MUX.lock
        //   ── while MutexGuard STILL HELD                ⇨ deadlock
        //                                                   (main thread
        //                                                   parked in
        //                                                   parking_lot::
        //                                                   RawMutex::
        //                                                   lock_slow,
        //                                                   beachball)
        //
        // Reproduces reliably by closing the last GUI tab on macOS when
        // a remote ClientDomain is registered: gui-startup spawns the
        // domain which adds a mux notification subscriber holding a weak
        // ref to ClientDomain; on app exit the FnOnce subscriber drops,
        // which drops ClientDomain, whose Drop calls Mux::try_get(). With
        // the implicit-temp form we deadlock on the same lock the outer
        // shutdown() is holding.
        let _taken = MUX.lock().take();
    }

    pub fn get() -> Arc<Mux> {
        Self::try_get().unwrap()
    }

    pub fn try_get() -> Option<Arc<Mux>> {
        MUX.lock().as_ref().map(Arc::clone)
    }

    pub fn get_pane(&self, pane_id: PaneId) -> Option<Arc<dyn Pane>> {
        self.panes.read().get(&pane_id).map(Arc::clone)
    }

    pub fn get_tab(&self, tab_id: TabId) -> Option<Arc<Tab>> {
        self.tabs.read().get(&tab_id).map(Arc::clone)
    }

    fn remove_pane_registration_if_same(&self, pane_id: PaneId, expected: &Arc<dyn Pane>) -> bool {
        let mut panes = self.panes.write();
        if panes
            .get(&pane_id)
            .is_some_and(|registered| Arc::ptr_eq(registered, expected))
        {
            panes.remove(&pane_id);
            true
        } else {
            false
        }
    }

    #[cfg(test)]
    fn remove_tab_registration_if_same(&self, tab_id: TabId, expected: &Arc<Tab>) -> bool {
        let mut tabs = self.tabs.write();
        if tabs
            .get(&tab_id)
            .is_some_and(|registered| Arc::ptr_eq(registered, expected))
        {
            tabs.remove(&tab_id);
            true
        } else {
            false
        }
    }

    /// Claim one pane ID for fallible external preparation.
    ///
    /// The claim prevents concurrent callers from consuming the same pane
    /// reader while allowing unrelated pane registrations to proceed. Public
    /// `Pane` callbacks are deliberately invoked only after this short map
    /// critical section has ended.
    fn claim_pane_preparation(
        &self,
        pane: &Arc<dyn Pane>,
    ) -> Result<Option<PanePreparationClaim<'_>>, Error> {
        let pane_id = pane.pane_id();
        let weak_pane = Arc::downgrade(pane);
        let generation = Arc::new(());
        let _registration = self.pane_registration.lock();
        if self.retiring_pane_ids.lock().contains(&pane_id) {
            return Err(PaneIdCollision { pane_id }.into());
        }
        let mut claims = self.pane_preparations.lock();
        if let Some(preparing) = claims.get(&pane_id) {
            if Weak::ptr_eq(&preparing.pane, &weak_pane) {
                return Err(PanePreparationInProgress { pane_id }.into());
            }
            return Err(PaneIdCollision { pane_id }.into());
        }

        if let Some(existing) = self.panes.read().get(&pane_id) {
            if Arc::ptr_eq(existing, pane) {
                return Ok(None);
            }
            return Err(PaneIdCollision { pane_id }.into());
        }

        claims.insert(
            pane_id,
            PanePreparation {
                pane: weak_pane.clone(),
                generation: Arc::clone(&generation),
            },
        );
        Ok(Some(PanePreparationClaim {
            registration: &self.pane_registration,
            claims: &self.pane_preparations,
            pane_id,
            pane: weak_pane,
            generation,
            active: true,
        }))
    }

    /// Cancel the current preparation for this ID, optionally requiring exact
    /// pane-instance identity. The caller must hold `pane_registration`.
    fn cancel_pane_preparation_locked(
        &self,
        pane_id: PaneId,
        expected: Option<&Arc<dyn Pane>>,
    ) -> bool {
        let mut claims = self.pane_preparations.lock();
        let should_cancel = claims.get(&pane_id).is_some_and(|preparing| {
            expected.is_none_or(|expected| Weak::ptr_eq(&preparing.pane, &Arc::downgrade(expected)))
        });
        if should_cancel {
            claims.remove(&pane_id);
        }
        should_cancel
    }

    /// Perform fallible pane callbacks after the caller owns the per-ID
    /// preparation claim and without holding the topology publication lock.
    fn prepare_claimed_pane_registration(
        &self,
        pane: &Arc<dyn Pane>,
        pane_id: PaneId,
    ) -> Result<PreparedPaneRegistration, Error> {
        let clipboard: Arc<dyn Clipboard> = Arc::new(MuxClipboard { pane_id });
        pane.set_clipboard(&clipboard);

        let downloader: Arc<dyn DownloadHandler> = Arc::new(MuxDownloader {});
        pane.set_download_handler(&downloader);

        let reader = pane.reader()?;
        Ok(PreparedPaneRegistration { pane_id, reader })
    }

    fn insert_pane_registration_locked(
        &self,
        pane_id: PaneId,
        pane: &Arc<dyn Pane>,
    ) -> Result<(), Error> {
        if self.retiring_pane_ids.lock().contains(&pane_id) {
            return Err(PaneIdCollision { pane_id }.into());
        }
        let mut panes = self.panes.write();
        if let Some(existing) = panes.get(&pane_id) {
            if Arc::ptr_eq(existing, pane) {
                return Err(anyhow!(
                    "pane identifier {pane_id} became registered during a serialized preparation"
                ));
            }
            return Err(PaneIdCollision { pane_id }.into());
        }
        panes.insert(pane_id, Arc::clone(pane));
        Ok(())
    }

    fn insert_tab_registration_locked(&self, tab: &Arc<Tab>) -> Result<bool, Error> {
        let tab_id = tab.tab_id();
        let mut tabs = self.tabs.write();
        if let Some(existing) = tabs.get(&tab_id) {
            if Arc::ptr_eq(existing, tab) {
                return Ok(false);
            }
            return Err(anyhow!(
                "tab identifier {tab_id} is already registered to a different tab instance"
            ));
        }
        tabs.insert(tab_id, Arc::clone(tab));
        Ok(true)
    }

    fn tab_registration_needs_insert_locked(&self, tab: &Arc<Tab>) -> Result<bool, Error> {
        let tab_id = tab.tab_id();
        if let Some(existing) = self.tabs.read().get(&tab_id) {
            if Arc::ptr_eq(existing, tab) {
                return Ok(false);
            }
            return Err(anyhow!(
                "tab identifier {tab_id} is already registered to a different tab instance"
            ));
        }
        Ok(true)
    }

    fn spawn_prepared_pane_reader(
        &self,
        pane: &Arc<dyn Pane>,
        prepared: PreparedPaneRegistration,
    ) -> Result<Option<PaneReaderStartGate>, Error> {
        let PreparedPaneRegistration { pane_id, reader } = prepared;
        if let Some(reader) = reader {
            let banner = self.banner.read().clone();
            let weak_pane = Arc::downgrade(pane);
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            thread::Builder::new()
                .name(format!("mux-read-pane-{pane_id}"))
                .spawn(move || {
                    if let Ok((weak_pane, banner, owner)) = release_rx.recv() {
                        read_from_pane_pty(owner, weak_pane, banner, reader);
                    }
                })
                .map_err(|err| {
                    anyhow!("failed to spawn pane reader thread for pane {pane_id}: {err}")
                })?;
            Ok(Some(PaneReaderStartGate {
                release_tx,
                pane_id,
                pane: weak_pane,
                banner,
            }))
        } else {
            Ok(None)
        }
    }

    pub fn add_pane(&self, pane: &Arc<dyn Pane>) -> Result<(), Error> {
        let Some(mut preparation_claim) = self.claim_pane_preparation(pane)? else {
            return Ok(());
        };
        let prepared = self.prepare_claimed_pane_registration(pane, preparation_claim.pane_id)?;
        let pane_id = prepared.pane_id;
        // Spawning is the last fallible external operation and therefore must
        // precede publication. The thread waits on its start gate.
        let mut reader_start_gate = self.spawn_prepared_pane_reader(pane, prepared)?;
        let publication_result = {
            let _registration = self.pane_registration.lock();
            let result = if preparation_claim.is_authoritative_locked() {
                self.insert_pane_registration_locked(pane_id, pane)
                    .map(|()| {
                        self.enqueue_pane_lifecycle_notification_locked(
                            PaneLifecycleNotification::Added(pane_id),
                            reader_start_gate.take(),
                        )
                    })
            } else {
                Err(PanePreparationCancelled { pane_id }.into())
            };
            preparation_claim.retire_locked();
            result
        };
        let lifecycle_notification = publication_result?;

        self.complete_pane_lifecycle_notification(lifecycle_notification);
        self.recompute_pane_count();
        Ok(())
    }

    pub fn add_tab_no_panes(&self, tab: &Arc<Tab>) -> Result<(), Error> {
        let inserted = {
            let _registration = self.pane_registration.lock();
            self.insert_tab_registration_locked(tab)?
        };
        if inserted {
            self.recompute_pane_count();
        }
        Ok(())
    }

    pub fn add_tab_and_active_pane(&self, tab: &Arc<Tab>) -> Result<(), Error> {
        let pane = tab
            .get_active_pane()
            .ok_or_else(|| anyhow!("tab MUST have an active pane"))?;
        let mut preparation_claim = self.claim_pane_preparation(&pane)?;
        let prepared = match preparation_claim.as_ref() {
            Some(claim) => Some(self.prepare_claimed_pane_registration(&pane, claim.pane_id)?),
            None => None,
        };
        let mut reader_start_gate = match prepared {
            Some(prepared) => self.spawn_prepared_pane_reader(&pane, prepared)?,
            None => None,
        };
        let publication_result = {
            let _registration = self.pane_registration.lock();
            let result = (|| -> Result<Option<PaneLifecycleNotificationTicket>, Error> {
                let tab_needs_insert = self.tab_registration_needs_insert_locked(tab)?;

                match preparation_claim.as_ref() {
                    None => {
                        let tab_was_inserted = self.insert_tab_registration_locked(tab)?;
                        debug_assert_eq!(tab_was_inserted, tab_needs_insert);
                        Ok(None)
                    }
                    Some(claim) => {
                        let pane_id = claim.pane_id;
                        if !claim.is_authoritative_locked() {
                            return Err(PanePreparationCancelled { pane_id }.into());
                        }
                        self.insert_pane_registration_locked(pane_id, &pane)?;
                        let tab_was_inserted = match self.insert_tab_registration_locked(tab) {
                            Ok(tab_was_inserted) => tab_was_inserted,
                            Err(err) => {
                                self.remove_pane_registration_if_same(pane_id, &pane);
                                return Err(err);
                            }
                        };
                        debug_assert_eq!(tab_was_inserted, tab_needs_insert);
                        if !tab_was_inserted && tab_needs_insert {
                            self.remove_pane_registration_if_same(pane_id, &pane);
                            return Err(anyhow!(
                                "tab identifier {} changed registration state during serialized publication",
                                tab.tab_id()
                            ));
                        }
                        let lifecycle_notification = self
                            .enqueue_pane_lifecycle_notification_locked(
                                PaneLifecycleNotification::Added(pane_id),
                                reader_start_gate.take(),
                            );
                        Ok(Some(lifecycle_notification))
                    }
                }
            })();
            if let Some(claim) = preparation_claim.as_mut() {
                claim.retire_locked();
            }
            result
        };
        let published_pane = publication_result?;

        if let Some(lifecycle_notification) = published_pane {
            self.complete_pane_lifecycle_notification(lifecycle_notification);
        }
        self.recompute_pane_count();
        Ok(())
    }

    fn take_pane_for_removal(
        &self,
        pane_id: PaneId,
        expected: Option<&Arc<dyn Pane>>,
    ) -> Option<RemovedPaneRegistration> {
        let (removed, needs_cleanup, cleanup_only_fence_owned) = {
            let _registration = self.pane_registration.lock();
            let preparation_cancelled = self.cancel_pane_preparation_locked(pane_id, expected);
            let pane = {
                let mut panes = self.panes.write();
                match expected {
                    Some(expected)
                        if panes
                            .get(&pane_id)
                            .is_some_and(|registered| Arc::ptr_eq(registered, expected)) =>
                    {
                        panes.remove(&pane_id)
                    }
                    Some(_) => None,
                    None => panes.remove(&pane_id),
                }
            };
            // An unqualified removal remains the authoritative stale-state
            // sweep even when no registry entry survives. Fence that cleanup
            // so it cannot erase state belonging to a concurrent replacement.
            let needs_cleanup = expected.is_none() || preparation_cancelled || pane.is_some();
            let fence_inserted = needs_cleanup && self.retiring_pane_ids.lock().insert(pane_id);
            let cleanup_only_fence_owned = fence_inserted && pane.is_none();
            let removed = pane.map(|pane| RemovedPaneRegistration {
                pane_id,
                pane,
                lifecycle_notification: self.enqueue_pane_lifecycle_notification_locked(
                    PaneLifecycleNotification::Removed(pane_id),
                    None,
                ),
            });
            (removed, needs_cleanup, cleanup_only_fence_owned)
        };

        if needs_cleanup {
            self.discard_removed_pane_states(&[pane_id]);
        }
        if cleanup_only_fence_owned {
            let _registration = self.pane_registration.lock();
            self.retiring_pane_ids.lock().remove(&pane_id);
        }
        removed
    }

    fn finish_pane_removal(&self, removed: RemovedPaneRegistration, kill: bool) {
        let RemovedPaneRegistration {
            pane_id,
            pane,
            lifecycle_notification,
        } = removed;
        if kill {
            log::debug!("killing pane {}", pane_id);
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| pane.kill())).is_err() {
                log::error!(
                    "pane {pane_id} panicked while being killed; completing removal lifecycle"
                );
            }
        }
        self.complete_pane_lifecycle_notification(lifecycle_notification);
    }

    fn remove_pane_internal(&self, pane_id: PaneId) {
        log::debug!("removing pane {}", pane_id);
        if let Some(removed) = self.take_pane_for_removal(pane_id, None) {
            self.finish_pane_removal(removed, true);
            self.recompute_pane_count();
        }
    }

    pub(crate) fn remove_pane_if_same(&self, pane_id: PaneId, expected: &Arc<dyn Pane>) {
        log::debug!("removing exact pane instance {}", pane_id);
        if let Some(removed) = self.take_pane_for_removal(pane_id, Some(expected)) {
            self.finish_pane_removal(removed, true);
            self.recompute_pane_count();
        }
    }

    fn take_tab_and_panes_for_removal(
        &self,
        tab_id: TabId,
    ) -> Option<(Arc<Tab>, Vec<RemovedPaneRegistration>)> {
        let tab = self.tabs.read().get(&tab_id).map(Arc::clone)?;
        let pane_candidates: Vec<(PaneId, Arc<dyn Pane>)> = tab
            .iter_all_panes()
            .into_iter()
            .map(|pane| (pane.pane_id(), pane))
            .collect();

        let removed_panes = {
            let _registration = self.pane_registration.lock();
            {
                let mut tabs = self.tabs.write();
                if !tabs
                    .get(&tab_id)
                    .is_some_and(|registered| Arc::ptr_eq(registered, &tab))
                {
                    return None;
                }
                tabs.remove(&tab_id);
            }
            for (pane_id, expected) in &pane_candidates {
                self.cancel_pane_preparation_locked(*pane_id, Some(expected));
            }
            let mut panes = self.panes.write();
            let removed_panes = pane_candidates
                .into_iter()
                .filter_map(|(pane_id, expected)| {
                    if panes
                        .get(&pane_id)
                        .is_some_and(|registered| Arc::ptr_eq(registered, &expected))
                    {
                        panes.remove(&pane_id).map(|pane| (pane_id, pane))
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();
            for (pane_id, _) in &removed_panes {
                self.retiring_pane_ids.lock().insert(*pane_id);
            }
            removed_panes
                .into_iter()
                .map(|(pane_id, pane)| RemovedPaneRegistration {
                    pane_id,
                    pane,
                    lifecycle_notification: self.enqueue_pane_lifecycle_notification_locked(
                        PaneLifecycleNotification::Removed(pane_id),
                        None,
                    ),
                })
                .collect::<Vec<_>>()
        };
        let pane_ids = removed_panes
            .iter()
            .map(|removed| removed.pane_id)
            .collect::<Vec<_>>();
        self.discard_removed_pane_states(&pane_ids);
        Some((tab, removed_panes))
    }

    fn remove_tab_internal(&self, tab_id: TabId) -> Option<Arc<Tab>> {
        log::debug!("remove_tab_internal tab {}", tab_id);

        let (tab, removed_panes) = self.take_tab_and_panes_for_removal(tab_id)?;

        if let Some(mut windows) = self.windows.try_write() {
            for w in windows.values_mut() {
                w.remove_by_id(tab_id);
            }
        }

        let pane_ids: Vec<PaneId> = removed_panes
            .iter()
            .map(|removed| removed.pane_id)
            .collect();
        log::debug!("panes to remove: {pane_ids:?}");
        for removed in removed_panes {
            self.finish_pane_removal(removed, true);
        }
        self.recompute_pane_count();

        Some(tab)
    }

    fn remove_window_internal(&self, window_id: WindowId) {
        log::debug!("remove_window_internal {}", window_id);

        let window = self.windows.write().remove(&window_id);
        if let Some(window) = window {
            // Gather all the domains referenced by this window
            let mut domains_of_window = HashSet::new();
            for tab in window.iter() {
                for pane in tab.iter_panes_ignoring_zoom() {
                    domains_of_window.insert(pane.pane.domain_id());
                }
            }

            for domain_id in domains_of_window {
                if let Some(domain) = self.get_domain(domain_id) {
                    if domain.detachable() {
                        log::info!("detaching domain");
                        if let Err(err) = domain.detach() {
                            log::error!(
                                "while detaching domain {domain_id} {}: {err:#}",
                                domain.domain_name()
                            );
                        }
                    }
                }
            }

            for tab in window.iter() {
                self.remove_tab_internal(tab.tab_id());
            }
            self.notify(MuxNotification::WindowRemoved(window_id));
        }
        self.recompute_pane_count();
    }

    pub fn remove_pane(&self, pane_id: PaneId) {
        self.remove_pane_internal(pane_id);
        self.prune_dead_windows();
    }

    pub fn remove_tab(&self, tab_id: TabId) -> Option<Arc<Tab>> {
        let tab = self.remove_tab_internal(tab_id);
        self.prune_dead_windows();
        tab
    }

    /// Drop the LOCAL mirror of a tab without disturbing the remote session.
    ///
    /// Mirrors [`Mux::remove_tab`] (registry removal, detach from every window,
    /// drop the tab's panes, prune now-empty windows) except that its batched
    /// pane removal does not call [`Pane::kill`], so no `Pdu::KillPane` is sent.
    /// This is the safety crux of the window-unify feature: when two local tabs
    /// mirror the same remote session, the duplicate's mirror is dropped here
    /// while the canonical window keeps its mirror and the remote session stays
    /// alive.
    pub fn remove_tab_local_only(&self, tab_id: TabId) -> Option<Arc<Tab>> {
        log::debug!("remove_tab_local_only tab {}", tab_id);

        let (tab, removed_panes) = self.take_tab_and_panes_for_removal(tab_id)?;

        if let Some(mut windows) = self.windows.try_write() {
            for w in windows.values_mut() {
                w.remove_by_id(tab_id);
            }
        }

        let pane_ids: Vec<PaneId> = removed_panes
            .iter()
            .map(|removed| removed.pane_id)
            .collect();
        log::debug!("panes to drop (local-only): {pane_ids:?}");
        for removed in removed_panes {
            self.finish_pane_removal(removed, false);
        }
        self.recompute_pane_count();
        self.prune_dead_windows();

        Some(tab)
    }

    pub fn prune_dead_windows(&self) {
        if Activity::count() > 0 {
            log::trace!("prune_dead_windows: Activity::count={}", Activity::count());
            return;
        }
        let live_tab_ids: Vec<TabId> = self.tabs.read().keys().cloned().collect();
        let mut dead_windows = vec![];
        let dead_tab_ids: Vec<TabId>;

        {
            let mut windows = match self.windows.try_write() {
                Some(w) => w,
                None => {
                    // It's ok if our caller already locked it; we can prune later.
                    log::trace!("prune_dead_windows: self.windows already borrowed");
                    return;
                }
            };
            for (window_id, win) in windows.iter_mut() {
                win.prune_dead_tabs(&live_tab_ids);
                if win.is_empty() {
                    log::trace!("prune_dead_windows: window is now empty");
                    dead_windows.push(*window_id);
                }
            }

            dead_tab_ids = self
                .tabs
                .read()
                .iter()
                .filter_map(|(&id, tab)| if tab.is_dead() { Some(id) } else { None })
                .collect();
        }

        for tab_id in dead_tab_ids {
            log::trace!("tab {} is dead", tab_id);
            self.remove_tab_internal(tab_id);
        }

        for window_id in dead_windows {
            log::trace!("window {} is dead", window_id);
            self.remove_window_internal(window_id);
        }

        if self.is_empty() {
            log::trace!("prune_dead_windows: is_empty, send MuxNotification::Empty");
            self.notify(MuxNotification::Empty);
        } else {
            log::trace!("prune_dead_windows: not empty");
        }
    }

    pub fn kill_window(&self, window_id: WindowId) {
        self.remove_window_internal(window_id);
        self.prune_dead_windows();
    }

    pub fn get_window(&self, window_id: WindowId) -> Option<MappedRwLockReadGuard<'_, Window>> {
        RwLockReadGuard::try_map(self.windows.read(), |windows| windows.get(&window_id)).ok()
    }

    pub fn get_window_mut(
        &self,
        window_id: WindowId,
    ) -> Option<MappedRwLockWriteGuard<'_, Window>> {
        RwLockWriteGuard::try_map(self.windows.write(), |windows| windows.get_mut(&window_id)).ok()
    }

    pub fn get_active_tab_for_window(&self, window_id: WindowId) -> Option<Arc<Tab>> {
        let window = self.get_window(window_id)?;
        window.get_active().map(Arc::clone)
    }

    pub fn window_has_panes_in_domain(&self, window_id: WindowId, domain_id: DomainId) -> bool {
        let Some(window) = self.get_window(window_id) else {
            return false;
        };

        for tab in window.iter() {
            if tab.has_panes_in_domain(domain_id) {
                return true;
            }
        }

        false
    }

    pub fn new_empty_window(
        &self,
        workspace: Option<String>,
        position: Option<GuiPosition>,
    ) -> MuxWindowBuilder {
        let workspace = Some(workspace.unwrap_or_else(|| self.active_workspace()));
        let window = Window::new(workspace, position);
        let window_id = window.window_id();
        self.windows.write().insert(window_id, window);
        MuxWindowBuilder {
            window_id,
            activity: Some(Activity::new()),
            notified: false,
        }
    }

    pub fn add_tab_to_window(&self, tab: &Arc<Tab>, window_id: WindowId) -> anyhow::Result<()> {
        let tab_id = tab.tab_id();
        {
            let mut window = self
                .get_window_mut(window_id)
                .ok_or_else(|| anyhow!("add_tab_to_window: no such window_id {}", window_id))?;
            window.push(tab);
        }
        self.recompute_pane_count();
        self.notify(MuxNotification::TabAddedToWindow { tab_id, window_id });
        Ok(())
    }

    pub fn window_containing_tab(&self, tab_id: TabId) -> Option<WindowId> {
        for w in self.windows.read().values() {
            for t in w.iter() {
                if t.tab_id() == tab_id {
                    return Some(w.window_id());
                }
            }
        }
        None
    }

    /// Move a tab from whichever window currently contains it into
    /// `dst_window` at position `idx` (appended when `idx` is `None`).
    ///
    /// This is a pure *metadata* move: the live `Arc<Tab>` (and all of its
    /// panes) is preserved in the mux tab registry and merely re-parented
    /// between the windows' ordered tab lists. No pane is killed and no
    /// `Pdu::KillPane` is sent -- this is the mechanism the window-unify
    /// feature uses to relocate non-duplicate tabs onto the canonical window.
    ///
    /// Window-lifecycle decisions (closing a now-empty source window) are left
    /// to the caller; this primitive does not prune. Workspace policy
    /// (same-workspace-only) is likewise enforced by the planner, not here.
    pub fn move_tab_between_windows(
        &self,
        tab_id: TabId,
        dst_window: WindowId,
        idx: Option<usize>,
    ) -> anyhow::Result<()> {
        let tab = self
            .tabs
            .read()
            .get(&tab_id)
            .map(Arc::clone)
            .ok_or_else(|| anyhow!("move_tab_between_windows: tab {tab_id} not found in mux"))?;
        let src_window = self.window_containing_tab(tab_id).ok_or_else(|| {
            anyhow!("move_tab_between_windows: tab {tab_id} is not in any window")
        })?;

        {
            let mut windows = self.windows.write();
            if !windows.contains_key(&dst_window) {
                return Err(anyhow!(
                    "move_tab_between_windows: destination window {dst_window} not found"
                ));
            }
            // Detach from the source window's ordered tab list first. When
            // src == dst this also lets us re-insert at `idx` (a reorder).
            // `Window::remove_by_id` only touches window-local bookkeeping; it
            // does NOT kill panes or remove the tab from `self.tabs`.
            if let Some(win) = windows.get_mut(&src_window) {
                win.remove_by_id(tab_id);
            }
            let dst = windows
                .get_mut(&dst_window)
                .expect("destination window presence checked above");
            let pos = idx.map(|i| i.min(dst.len())).unwrap_or_else(|| dst.len());
            dst.insert(pos, &tab);
        }

        // Pane count is unchanged for a within-workspace move; recompute keeps
        // the per-workspace tallies correct if a caller ever moves across
        // workspaces.
        self.recompute_pane_count();
        self.notify(MuxNotification::TabAddedToWindow {
            tab_id,
            window_id: dst_window,
        });
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.panes.read().is_empty()
    }

    pub fn is_workspace_empty(&self, workspace: &str) -> bool {
        *self
            .num_panes_by_workspace
            .read()
            .get(workspace)
            .unwrap_or(&0)
            == 0
    }

    pub fn is_active_workspace_empty(&self) -> bool {
        let workspace = self.active_workspace();
        self.is_workspace_empty(&workspace)
    }

    pub fn iter_panes(&self) -> Vec<Arc<dyn Pane>> {
        self.panes
            .read()
            .iter()
            .map(|(_, v)| Arc::clone(v))
            .collect()
    }

    pub fn iter_windows_in_workspace(&self, workspace: &str) -> Vec<WindowId> {
        let mut windows: Vec<WindowId> = self
            .windows
            .read()
            .iter()
            .filter_map(|(k, w)| {
                if w.get_workspace() == workspace {
                    Some(k)
                } else {
                    None
                }
            })
            .cloned()
            .collect();
        windows.sort();
        windows
    }

    pub fn iter_windows(&self) -> Vec<WindowId> {
        self.windows.read().keys().cloned().collect()
    }

    pub fn iter_domains(&self) -> Vec<Arc<dyn Domain>> {
        self.domains.read().values().cloned().collect()
    }

    pub fn resolve_pane_id(&self, pane_id: PaneId) -> Option<(DomainId, WindowId, TabId)> {
        let mut ids = None;
        for tab in self.tabs.read().values() {
            if let Some(domain_id) = tab.domain_id_for_pane(pane_id) {
                ids = Some((tab.tab_id(), domain_id));
                break;
            }
        }
        let (tab_id, domain_id) = ids?;
        let window_id = self.window_containing_tab(tab_id)?;
        Some((domain_id, window_id, tab_id))
    }

    pub fn domain_was_detached(&self, domain: DomainId) {
        let Some(expected) = self.get_domain(domain) else {
            return;
        };
        let _ = self.domain_was_detached_if_same(expected.as_ref());
    }

    /// Remove one exact domain instance and all of its topology.
    ///
    /// Domain identifiers are retired before topology callbacks run, but the
    /// exact detached domain remains discoverable until its panes have been
    /// killed. `ClientPane::kill` relies on that detached registration to
    /// suppress an unintended remote `KillPane` during transport teardown.
    pub fn domain_was_detached_if_same(&self, expected: &dyn Domain) -> bool {
        let domain = expected.domain_id();
        let removed = {
            let _registration = self.domain_registration.lock();
            let Some(registered) = self.domains.read().get(&domain).cloned() else {
                return false;
            };
            if !std::ptr::eq(registered.as_ref(), expected) {
                return false;
            }
            if !self.retired_domain_ids.lock().insert(domain) {
                // Another exact teardown already owns this retired
                // registration. Do not duplicate pane kills or callbacks.
                return false;
            }
            registered
        };

        // Tmux domains install mux notification subscriptions that should be
        // removed eagerly when the domain is detached. Waiting for the next
        // notification to lazily retain-drop stale callbacks can leak
        // subscribers in long-idle sessions.
        if let Some(tmux_domain) = removed.downcast_ref::<TmuxDomain>() {
            if let Some(sub_id) = tmux_domain.inner.notification_sub_id.lock().take() {
                let _ = self.unsubscribe(sub_id);
            }
        }

        let mut dead_panes = vec![];
        for pane in self.panes.read().values() {
            if pane.domain_id() == domain {
                dead_panes.push(pane.pane_id());
            }
        }

        {
            let mut windows = self.windows.write();
            for win in windows.values_mut() {
                for tab in win.iter() {
                    tab.kill_panes_in_domain(domain);
                }
            }
        }

        log::info!("domain detached panes: {:?}", dead_panes);
        for pane_id in dead_panes {
            self.remove_pane_internal(pane_id);
        }

        self.prune_dead_windows();

        {
            let _registration = self.domain_registration.lock();
            let mut domains = self.domains.write();
            if !domains
                .get(&domain)
                .is_some_and(|current| Arc::ptr_eq(current, &removed))
            {
                log::error!(
                    "retired domain {domain} changed identity during exact teardown; preserving \
                     the unexpected registration"
                );
                return false;
            }
            domains.remove(&domain);
            drop(domains);

            self.domains_by_name
                .write()
                .retain(|_, current| !Arc::ptr_eq(current, &removed));

            let should_replace_default = self
                .default_domain
                .read()
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &removed));
            if should_replace_default {
                let replacement = self.domains.read().values().next().cloned();
                *self.default_domain.write() = replacement;
            }
        }
        true
    }

    pub fn set_banner(&self, banner: Option<String>) {
        *self.banner.write() = banner;
    }

    pub fn resolve_spawn_tab_domain(
        &self,
        source_pane_id: Option<PaneId>,
        domain: &config::keyassignment::SpawnTabDomain,
    ) -> anyhow::Result<Arc<dyn Domain>> {
        let domain = match domain {
            SpawnTabDomain::DefaultDomain => self.resolve_default_domain()?,
            SpawnTabDomain::CurrentPaneDomain => match source_pane_id {
                Some(pane_id) => {
                    let (pane_domain_id, _window_id, _tab_id) = self
                        .resolve_pane_id(pane_id)
                        .ok_or_else(|| anyhow!("pane_id {} invalid", pane_id))?;
                    self.get_domain(pane_domain_id).ok_or_else(|| {
                        anyhow!("pane_id {pane_id} resolved to missing domain id {pane_domain_id}")
                    })?
                }
                None => self.resolve_default_domain()?,
            },
            SpawnTabDomain::DomainId(domain_id) => self
                .get_domain(*domain_id)
                .ok_or_else(|| anyhow!("domain id {} is invalid", domain_id))?,
            SpawnTabDomain::DomainName(name) => {
                self.get_domain_by_name(&name).ok_or_else(|| {
                    let names: Vec<String> = self
                        .domains_by_name
                        .read()
                        .keys()
                        .map(|name| format!("\"{name}\""))
                        .collect();
                    anyhow!(
                        "domain name \"{name}\" is invalid. Possible names are {}.",
                        names.join(", ")
                    )
                })?
            }
        };
        Ok(domain)
    }

    fn resolve_cwd(
        &self,
        command_dir: Option<String>,
        pane: Option<Arc<dyn Pane>>,
        target_domain: DomainId,
        policy: CachePolicy,
    ) -> Option<String> {
        command_dir.or_else(|| {
            match pane {
                Some(pane) if pane.domain_id() == target_domain => pane
                    .get_current_working_dir(policy)
                    .and_then(|url| {
                        percent_decode_str(url.path())
                            .decode_utf8()
                            .ok()
                            .map(|path| path.into_owned())
                    })
                    .map(|path| {
                        // On Windows the file URI can produce a path like:
                        // `/C:\Users` which is valid in a file URI, but the leading slash
                        // is not liked by the windows file APIs, so we strip it off here.
                        let bytes = path.as_bytes();
                        if bytes.len() > 2 && bytes[0] == b'/' && bytes[2] == b':' {
                            path[1..].to_owned()
                        } else {
                            path
                        }
                    }),
                _ => None,
            }
        })
    }

    pub async fn split_pane(
        &self,
        source_pane_id: PaneId,
        request: SplitRequest,
        source: SplitSource,
        domain: config::keyassignment::SpawnTabDomain,
    ) -> anyhow::Result<(Arc<dyn Pane>, TerminalSize)> {
        let (_pane_domain_id, window_id, tab_id) = self
            .resolve_pane_id(source_pane_id)
            .ok_or_else(|| anyhow!("pane_id {} invalid", source_pane_id))?;

        let domain = self
            .resolve_spawn_tab_domain(Some(source_pane_id), &domain)
            .context("resolve_spawn_tab_domain")?;

        if domain.state() == DomainState::Detached {
            domain.attach(Some(window_id)).await?;
        }

        let current_pane = self
            .get_pane(source_pane_id)
            .ok_or_else(|| anyhow!("pane_id {} is invalid", source_pane_id))?;
        let term_config = current_pane.get_config();

        let source = match source {
            SplitSource::Spawn {
                command,
                command_dir,
            } => SplitSource::Spawn {
                command,
                command_dir: self.resolve_cwd(
                    command_dir,
                    Some(Arc::clone(&current_pane)),
                    domain.domain_id(),
                    CachePolicy::FetchImmediate,
                ),
            },
            other => other,
        };

        let pane = domain
            .split_pane(source, tab_id, source_pane_id, request)
            .await?;
        if let Some(config) = term_config {
            pane.set_config(config);
        }

        let dims = pane.get_dimensions();

        let size = TerminalSize {
            cols: dims.cols,
            rows: dims.viewport_rows,
            pixel_height: dims.pixel_height,
            pixel_width: dims.pixel_width,
            dpi: dims.dpi,
        };

        Ok((pane, size))
    }

    pub async fn move_pane_to_new_tab(
        &self,
        pane_id: PaneId,
        window_id: Option<WindowId>,
        workspace_for_new_window: Option<String>,
    ) -> anyhow::Result<(Arc<Tab>, WindowId)> {
        let (domain_id, _src_window, src_tab) = self
            .resolve_pane_id(pane_id)
            .ok_or_else(|| anyhow::anyhow!("pane {} not found", pane_id))?;

        let domain = self
            .get_domain(domain_id)
            .ok_or_else(|| anyhow::anyhow!("domain {domain_id} of pane {pane_id} not found"))?;

        if let Some((tab, window_id)) = domain
            .move_pane_to_new_tab(pane_id, window_id, workspace_for_new_window.clone())
            .await?
        {
            return Ok((tab, window_id));
        }

        let src_tab = match self.get_tab(src_tab) {
            Some(t) => t,
            None => anyhow::bail!("Invalid tab id {}", src_tab),
        };

        let window_builder;
        let (window_id, size) = if let Some(window_id) = window_id {
            let window = self
                .get_window_mut(window_id)
                .ok_or_else(|| anyhow!("window_id {} not found on this server", window_id))?;
            let tab = window
                .get_active()
                .ok_or_else(|| anyhow!("window {} has no tabs", window_id))?;
            let size = tab.get_size();

            (window_id, size)
        } else {
            window_builder = self.new_empty_window(workspace_for_new_window, None);
            (*window_builder, src_tab.get_size())
        };

        let pane = src_tab
            .remove_pane(pane_id)
            .ok_or_else(|| anyhow::anyhow!("pane {} wasn't in its containing tab!?", pane_id))?;

        let tab = Arc::new(Tab::new(&size));
        tab.assign_pane(&pane);
        pane.resize(size)?;
        self.add_tab_and_active_pane(&tab)?;
        self.add_tab_to_window(&tab, window_id)?;

        if src_tab.is_dead() {
            self.remove_tab(src_tab.tab_id());
        }

        Ok((tab, window_id))
    }

    pub async fn spawn_tab_or_window(
        &self,
        window_id: Option<WindowId>,
        domain: SpawnTabDomain,
        command: Option<CommandBuilder>,
        command_dir: Option<String>,
        size: TerminalSize,
        current_pane_id: Option<PaneId>,
        workspace_for_new_window: String,
        window_position: Option<GuiPosition>,
    ) -> anyhow::Result<(Arc<Tab>, Arc<dyn Pane>, WindowId)> {
        let domain = self
            .resolve_spawn_tab_domain(current_pane_id, &domain)
            .context("resolve_spawn_tab_domain")?;

        let window_builder;
        let term_config;

        let (window_id, size) = if let Some(window_id) = window_id {
            let window = self
                .get_window_mut(window_id)
                .ok_or_else(|| anyhow!("window_id {} not found on this server", window_id))?;
            let tab = window
                .get_active()
                .ok_or_else(|| anyhow!("window {} has no tabs", window_id))?;
            let pane = tab
                .get_active_pane()
                .ok_or_else(|| anyhow!("active tab in window {} has no panes", window_id))?;
            term_config = pane.get_config();

            let size = tab.get_size();

            (window_id, size)
        } else {
            term_config = None;
            window_builder = self.new_empty_window(Some(workspace_for_new_window), window_position);
            (*window_builder, size)
        };

        if domain.state() == DomainState::Detached {
            domain.attach(Some(window_id)).await?;
        }

        let cwd = self.resolve_cwd(
            command_dir,
            match current_pane_id {
                Some(id) => {
                    // Only use the cwd from the current pane if the domain
                    // is the same as the one we are spawning into
                    let (current_domain_id, _, _) = self
                        .resolve_pane_id(id)
                        .ok_or_else(|| anyhow!("pane_id {} invalid", id))?;
                    if current_domain_id == domain.domain_id() {
                        self.get_pane(id)
                    } else {
                        None
                    }
                }
                None => None,
            },
            domain.domain_id(),
            CachePolicy::FetchImmediate,
        );

        let tab = domain
            .spawn(size, command.clone(), cwd.clone(), window_id)
            .await
            .with_context(|| {
                format!(
                    "Spawning in domain `{}`: {size:?} command={command:?} cwd={cwd:?}",
                    domain.domain_name()
                )
            })?;

        let pane = tab
            .get_active_pane()
            .ok_or_else(|| anyhow!("missing active pane on tab!?"))?;

        if let Some(config) = term_config {
            pane.set_config(config);
        }

        let mut window = self
            .get_window_mut(window_id)
            .ok_or_else(|| anyhow!("no such window!?"))?;
        if let Some(idx) = window.idx_by_id(tab.tab_id()) {
            window.save_and_then_set_active(idx);
        }

        Ok((tab, pane, window_id))
    }
}

pub struct IdentityHolder {
    prior: Option<Arc<ClientId>>,
}

impl Drop for IdentityHolder {
    fn drop(&mut self) {
        if let Some(mux) = Mux::try_get() {
            mux.replace_identity(self.prior.take());
        }
    }
}

#[derive(Debug, Error)]
#[allow(dead_code)]
pub enum SessionTerminated {
    #[error("Process exited: {:?}", status)]
    ProcessStatus { status: ExitStatus },
    #[error("Error: {:?}", err)]
    Error { err: Error },
    #[error("Window Closed")]
    WindowClosed,
}

pub(crate) fn terminal_size_to_pty_size(size: TerminalSize) -> anyhow::Result<PtySize> {
    Ok(PtySize {
        rows: size.rows.try_into()?,
        cols: size.cols.try_into()?,
        pixel_height: size.pixel_height.try_into()?,
        pixel_width: size.pixel_width.try_into()?,
    })
}

struct MuxClipboard {
    pane_id: PaneId,
}

impl Clipboard for MuxClipboard {
    fn set_contents(
        &self,
        selection: ClipboardSelection,
        clipboard: Option<String>,
    ) -> anyhow::Result<()> {
        let mux =
            Mux::try_get().ok_or_else(|| anyhow::anyhow!("MuxClipboard::set_contents: no Mux?"))?;
        mux.notify(MuxNotification::AssignClipboard {
            pane_id: self.pane_id,
            selection,
            clipboard,
        });
        Ok(())
    }
}

struct MuxDownloader {}

impl frankenterm_term::DownloadHandler for MuxDownloader {
    fn save_to_downloads(&self, name: Option<String>, data: Vec<u8>) {
        if let Some(mux) = Mux::try_get() {
            mux.notify(MuxNotification::SaveToDownloads {
                name,
                data: Arc::new(data),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pane::{ForEachPaneLogicalLine, LogicalLine, WithPaneLines};
    use crate::renderable::{RenderableDimensions, StableCursorPosition};
    use frankenterm_term::color::ColorPalette;
    use frankenterm_term::{KeyCode, KeyModifiers, MouseEvent, StableRowIndex, TerminalSize};
    use parking_lot::{MappedMutexGuard, MutexGuard};
    use proptest::prelude::*;
    use rangeset::RangeSet;
    use std::ops::Range;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::MutexGuard as StdMutexGuard;
    use termwiz::surface::{Line, SequenceNo};

    fn global_test_lock() -> StdMutexGuard<'static, ()> {
        crate::MUX_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner())
    }

    struct KillCountingPane {
        id: PaneId,
        size: Mutex<TerminalSize>,
        kills: Arc<AtomicUsize>,
        writes: Mutex<Vec<u8>>,
        reader: Mutex<Option<Box<dyn std::io::Read + Send>>>,
        on_reader: Mutex<Option<Box<dyn FnOnce() + Send>>>,
        on_kill: Mutex<Option<Box<dyn FnOnce() + Send>>>,
        fail_reader: bool,
    }

    impl KillCountingPane {
        fn new(id: PaneId, size: TerminalSize) -> (Arc<dyn Pane>, Arc<AtomicUsize>) {
            Self::new_with_reader(id, size, None, false)
        }

        fn new_with_reader(
            id: PaneId,
            size: TerminalSize,
            reader: Option<Box<dyn std::io::Read + Send>>,
            fail_reader: bool,
        ) -> (Arc<dyn Pane>, Arc<AtomicUsize>) {
            let kills = Arc::new(AtomicUsize::new(0));
            let pane: Arc<dyn Pane> = Arc::new(Self {
                id,
                size: Mutex::new(size),
                kills: Arc::clone(&kills),
                writes: Mutex::new(Vec::new()),
                reader: Mutex::new(reader),
                on_reader: Mutex::new(None),
                on_kill: Mutex::new(None),
                fail_reader,
            });
            (pane, kills)
        }

        fn new_with_reader_callback(
            id: PaneId,
            size: TerminalSize,
            on_reader: impl FnOnce() + Send + 'static,
        ) -> (Arc<dyn Pane>, Arc<AtomicUsize>) {
            let kills = Arc::new(AtomicUsize::new(0));
            let pane: Arc<dyn Pane> = Arc::new(Self {
                id,
                size: Mutex::new(size),
                kills: Arc::clone(&kills),
                writes: Mutex::new(Vec::new()),
                reader: Mutex::new(None),
                on_reader: Mutex::new(Some(Box::new(on_reader))),
                on_kill: Mutex::new(None),
                fail_reader: false,
            });
            (pane, kills)
        }

        fn new_with_kill_callback(
            id: PaneId,
            size: TerminalSize,
            on_kill: impl FnOnce() + Send + 'static,
        ) -> (Arc<dyn Pane>, Arc<AtomicUsize>) {
            let kills = Arc::new(AtomicUsize::new(0));
            let pane: Arc<dyn Pane> = Arc::new(Self {
                id,
                size: Mutex::new(size),
                kills: Arc::clone(&kills),
                writes: Mutex::new(Vec::new()),
                reader: Mutex::new(None),
                on_reader: Mutex::new(None),
                on_kill: Mutex::new(Some(Box::new(on_kill))),
                fail_reader: false,
            });
            (pane, kills)
        }
    }

    impl Pane for KillCountingPane {
        fn pane_id(&self) -> PaneId {
            self.id
        }

        fn get_cursor_position(&self) -> StableCursorPosition {
            StableCursorPosition::default()
        }

        fn get_current_seqno(&self) -> SequenceNo {
            0
        }

        fn get_changed_since(
            &self,
            _lines: Range<StableRowIndex>,
            _seqno: SequenceNo,
        ) -> RangeSet<StableRowIndex> {
            RangeSet::new()
        }

        fn get_lines(&self, _lines: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
            (0, Vec::new())
        }

        fn with_lines_mut(
            &self,
            _lines: Range<StableRowIndex>,
            _with_lines: &mut dyn WithPaneLines,
        ) {
        }

        fn for_each_logical_line_in_stable_range_mut(
            &self,
            _lines: Range<StableRowIndex>,
            _for_line: &mut dyn ForEachPaneLogicalLine,
        ) {
        }

        fn get_logical_lines(&self, _lines: Range<StableRowIndex>) -> Vec<LogicalLine> {
            Vec::new()
        }

        fn get_dimensions(&self) -> RenderableDimensions {
            let size = *self.size.lock();
            RenderableDimensions {
                cols: size.cols,
                viewport_rows: size.rows,
                scrollback_rows: size.rows,
                physical_top: 0,
                scrollback_top: 0,
                dpi: size.dpi,
                pixel_width: size.pixel_width,
                pixel_height: size.pixel_height,
                reverse_video: false,
            }
        }

        fn get_title(&self) -> String {
            format!("kill-counting-pane-{}", self.id)
        }

        fn send_paste(&self, _text: &str) -> anyhow::Result<()> {
            Ok(())
        }

        fn reader(&self) -> anyhow::Result<Option<Box<dyn std::io::Read + Send>>> {
            if let Some(on_reader) = self.on_reader.lock().take() {
                on_reader();
            }
            if self.fail_reader {
                return Err(anyhow!("intentional test pane reader acquisition failure"));
            }
            Ok(self.reader.lock().take())
        }

        fn writer(&self) -> MappedMutexGuard<'_, dyn std::io::Write> {
            MutexGuard::map(self.writes.lock(), |writes| {
                let writer: &mut dyn std::io::Write = writes;
                writer
            })
        }

        fn resize(&self, size: TerminalSize) -> anyhow::Result<()> {
            *self.size.lock() = size;
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

        fn kill(&self) {
            self.kills.fetch_add(1, Ordering::SeqCst);
            if let Some(on_kill) = self.on_kill.lock().take() {
                on_kill();
            }
        }

        fn palette(&self) -> ColorPalette {
            ColorPalette::default()
        }

        fn domain_id(&self) -> DomainId {
            1
        }

        fn is_mouse_grabbed(&self) -> bool {
            false
        }

        fn is_alt_screen_active(&self) -> bool {
            false
        }

        fn get_current_working_dir(&self, _policy: CachePolicy) -> Option<url::Url> {
            None
        }
    }

    struct RegistrationObservingReader {
        mux: Arc<Mux>,
        pane_id: PaneId,
        tab_id: TabId,
        pane_added: Arc<AtomicBool>,
        result_tx: Option<std::sync::mpsc::Sender<(bool, bool, bool)>>,
    }

    impl std::io::Read for RegistrationObservingReader {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            if let Some(result_tx) = self.result_tx.take() {
                let pane_is_registered = self.mux.get_pane(self.pane_id).is_some();
                let tab_contains_pane = self
                    .mux
                    .get_tab(self.tab_id)
                    .and_then(|tab| tab.get_active_pane())
                    .is_some_and(|pane| pane.pane_id() == self.pane_id);
                let pane_added_was_emitted = self.pane_added.load(Ordering::SeqCst);
                let _ = result_tx.send((
                    pane_is_registered,
                    tab_contains_pane,
                    pane_added_was_emitted,
                ));
            }
            Ok(0)
        }
    }

    struct CancellationObservingReader {
        reads: Arc<AtomicUsize>,
        dropped_tx: Option<std::sync::mpsc::Sender<()>>,
    }

    impl std::io::Read for CancellationObservingReader {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            Ok(0)
        }
    }

    impl Drop for CancellationObservingReader {
        fn drop(&mut self) {
            if let Some(dropped_tx) = self.dropped_tx.take() {
                let _ = dropped_tx.send(());
            }
        }
    }

    fn test_size() -> TerminalSize {
        TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        }
    }

    fn register_test_pane(mux: &Mux, pane_id: PaneId) -> Arc<dyn Pane> {
        let (pane, _) = KillCountingPane::new(pane_id, test_size());
        mux.add_pane(&pane)
            .expect("test pane should register with mux");
        pane
    }

    fn pane_with_blocked_reader(
        pane_id: PaneId,
    ) -> (
        Arc<dyn Pane>,
        Arc<AtomicUsize>,
        std::sync::mpsc::Receiver<()>,
        std::sync::mpsc::Sender<()>,
    ) {
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (pane, kills) =
            KillCountingPane::new_with_reader_callback(pane_id, test_size(), move || {
                entered_tx
                    .send(())
                    .expect("registration thread should report reader acquisition");
                release_rx
                    .recv_timeout(Duration::from_secs(30))
                    .expect("test should release blocked reader acquisition");
            });
        (pane, kills, entered_rx, release_tx)
    }

    fn tab_with_kill_counter(mux: &Mux, pane_id: PaneId) -> (Arc<Tab>, Arc<AtomicUsize>) {
        let size = test_size();
        let tab = Arc::new(Tab::new(&size));
        let (pane, kills) = KillCountingPane::new(pane_id, size);
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab)
            .expect("test tab should register with mux");
        (tab, kills)
    }

    #[test]
    fn add_pane_rejects_a_different_instance_with_the_same_id() {
        let mux = Mux::new(None);
        let (first, _) = KillCountingPane::new(77, test_size());
        let (duplicate, _) = KillCountingPane::new(77, test_size());

        mux.add_pane(&first)
            .expect("first pane instance should register");
        let err = mux
            .add_pane(&duplicate)
            .expect_err("a different pane instance must not replace the registered pane");
        let collision = err
            .downcast_ref::<PaneIdCollision>()
            .expect("duplicate registration should preserve its typed error");
        assert_eq!(collision.pane_id, 77);

        let registered = mux
            .get_pane(77)
            .expect("first pane should remain registered");
        assert!(Arc::ptr_eq(&registered, &first));
        assert_eq!(mux.panes.read().len(), 1);
    }

    #[test]
    fn standalone_mux_reader_is_not_misclassified_as_stale_without_a_global_owner() {
        let _guard = global_test_lock();
        Mux::shutdown();
        let mux = Mux::new(None);
        let (pane, _) = KillCountingPane::new(75, test_size());
        mux.add_pane(&pane)
            .expect("standalone pane should register");
        let dead = Arc::new(AtomicBool::new(false));

        send_actions_to_mux(&None, &Arc::downgrade(&pane), &dead, Vec::new());

        assert!(
            !dead.load(Ordering::Acquire),
            "absence of a global mux must not terminate a valid standalone reader",
        );
    }

    #[test]
    fn same_instance_preparation_conflict_is_typed_and_retryable() {
        let mux = Mux::new(None);
        let (pane, _) = KillCountingPane::new(76, test_size());
        let preparation_claim = mux
            .claim_pane_preparation(&pane)
            .expect("first preparation claim should succeed")
            .expect("an unregistered pane should require preparation");

        let err = match mux.claim_pane_preparation(&pane) {
            Err(err) => err,
            Ok(_) => panic!("same-instance concurrent preparation must report busy"),
        };
        assert_eq!(
            err.downcast_ref::<PanePreparationInProgress>(),
            Some(&PanePreparationInProgress { pane_id: 76 })
        );
        assert!(mux.get_pane(76).is_none());
        drop(preparation_claim);
    }

    #[test]
    fn removal_cancels_only_the_claimed_generation_before_pane_publication() {
        let mux = Arc::new(Mux::new(None));
        let (pane, kills, reader_entered, release_reader) = pane_with_blocked_reader(86);
        let mux_for_add = Arc::clone(&mux);
        let pane_for_add = Arc::clone(&pane);
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let add_thread = std::thread::spawn(move || {
            let result = mux_for_add.add_pane(&pane_for_add);
            result_tx
                .send(result)
                .expect("test should still be waiting for registration result");
        });

        reader_entered
            .recv_timeout(Duration::from_secs(30))
            .expect("pane preparation should reach its blocking reader callback");
        mux.remove_pane(86);
        let replacement_claim = mux
            .claim_pane_preparation(&pane)
            .expect("the same pane may claim a new generation after cancellation")
            .expect("the cancelled pane was never published");
        release_reader
            .send(())
            .expect("blocked reader callback should still be waiting");

        let err = result_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("cancelled registration should finish")
            .expect_err("the stale preparation generation must not publish");
        assert_eq!(
            err.downcast_ref::<PanePreparationCancelled>(),
            Some(&PanePreparationCancelled { pane_id: 86 })
        );
        add_thread
            .join()
            .expect("registration thread should not panic");
        assert!(
            mux.get_pane(86).is_none(),
            "removal must fence stale publication"
        );
        assert_eq!(kills.load(Ordering::SeqCst), 0);

        let retry_err = match mux.claim_pane_preparation(&pane) {
            Err(err) => err,
            Ok(_) => panic!("stale completion must preserve the newer preparation claim"),
        };
        assert_eq!(
            retry_err.downcast_ref::<PanePreparationInProgress>(),
            Some(&PanePreparationInProgress { pane_id: 86 })
        );
        drop(replacement_claim);
        assert!(mux.pane_preparations.lock().is_empty());
    }

    #[test]
    fn exact_instance_removal_does_not_cancel_a_different_preparation() {
        let mux = Arc::new(Mux::new(None));
        let (pane, kills, reader_entered, release_reader) = pane_with_blocked_reader(87);
        let (different_instance, different_kills) = KillCountingPane::new(87, test_size());
        let mux_for_add = Arc::clone(&mux);
        let pane_for_add = Arc::clone(&pane);
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let add_thread = std::thread::spawn(move || {
            let result = mux_for_add.add_pane(&pane_for_add);
            result_tx
                .send(result)
                .expect("test should still be waiting for registration result");
        });

        reader_entered
            .recv_timeout(Duration::from_secs(30))
            .expect("pane preparation should reach its blocking reader callback");
        mux.remove_pane_if_same(87, &different_instance);
        release_reader
            .send(())
            .expect("blocked reader callback should still be waiting");
        result_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("uncancelled registration should finish")
            .expect("identity-mismatched removal must not cancel preparation");
        add_thread
            .join()
            .expect("registration thread should not panic");

        let registered = mux
            .get_pane(87)
            .expect("the exact claimed pane should be published");
        assert!(Arc::ptr_eq(&registered, &pane));
        assert_eq!(kills.load(Ordering::SeqCst), 0);
        assert_eq!(different_kills.load(Ordering::SeqCst), 0);
        assert!(mux.pane_preparations.lock().is_empty());
    }

    #[test]
    fn pane_lifecycle_observers_follow_serialized_topology_order() {
        let mux = Arc::new(Mux::new(None));
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observed_for_subscriber = Arc::clone(&observed);
        let mux_for_subscriber = Arc::downgrade(&mux);
        mux.subscribe(move |notification| {
            let transition = match notification {
                MuxNotification::PaneAdded(89) => Some("added"),
                MuxNotification::PaneRemoved(89) => Some("removed"),
                _ => None,
            };
            if let Some(transition) = transition {
                let mux = mux_for_subscriber
                    .upgrade()
                    .expect("test mux should outlive its subscriber");
                assert!(
                    mux.pane_registration.try_lock().is_some(),
                    "lifecycle observers must run outside pane_registration"
                );
                assert!(
                    mux.subscribers.try_write().is_some(),
                    "lifecycle observers must run outside the subscriber registry lock"
                );
                observed_for_subscriber.lock().push(transition);
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");

        // Both mutators block in count recomputation after publishing their
        // lifecycle event. Count aggregation must not stall reader start or
        // later ordered lifecycle delivery.
        let pane_count_guard = mux.num_panes_by_workspace.read();
        let (pane, kills) = KillCountingPane::new(89, test_size());
        let mux_for_add = Arc::clone(&mux);
        let pane_for_add = Arc::clone(&pane);
        let (add_tx, add_rx) = std::sync::mpsc::channel();
        let add_thread = std::thread::spawn(move || {
            let result = mux_for_add.add_pane(&pane_for_add);
            add_tx
                .send(result)
                .expect("test should still be waiting for add result");
        });

        let publication_deadline = Instant::now() + Duration::from_secs(30);
        while mux.get_pane(89).is_none() {
            assert!(
                Instant::now() < publication_deadline,
                "add should publish before blocking in pane-count recomputation"
            );
            std::thread::yield_now();
        }
        // Registry insertion and lifecycle-ticket completion are separate
        // linearization points; the reader gate makes the latter authoritative
        // for external observation. Wait for that boundary explicitly.
        while observed.lock().as_slice() != ["added"] {
            assert!(
                Instant::now() < publication_deadline,
                "PaneAdded should publish before blocking in pane-count recomputation"
            );
            std::thread::yield_now();
        }
        assert_eq!(
            &*observed.lock(),
            &["added"],
            "PaneAdded must not wait behind count aggregation",
        );

        let mux_for_remove = Arc::clone(&mux);
        let (remove_tx, remove_rx) = std::sync::mpsc::channel();
        let remove_thread = std::thread::spawn(move || {
            mux_for_remove.remove_pane(89);
            remove_tx
                .send(())
                .expect("test should still be waiting for remove result");
        });
        let removal_deadline = Instant::now() + Duration::from_secs(30);
        while mux.get_pane(89).is_some() {
            assert!(
                Instant::now() < removal_deadline,
                "remove should mutate topology before blocking in pane-count recomputation"
            );
            std::thread::yield_now();
        }
        // Topology removal linearizes before the fallible/panicking Pane::kill
        // callback and before its lifecycle ticket becomes ready. Wait for the
        // distinct observer boundary rather than assuming that seeing the map
        // mutation means the removal thread has already completed both steps.
        while observed.lock().as_slice() != ["added", "removed"] {
            assert!(
                Instant::now() < removal_deadline,
                "PaneRemoved should publish before blocking in pane-count recomputation"
            );
            std::thread::yield_now();
        }
        assert_eq!(
            &*observed.lock(),
            &["added", "removed"],
            "a later removal must follow the earlier addition without waiting on count aggregation"
        );

        drop(pane_count_guard);
        add_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("add should finish after pane-count barrier release")
            .expect("serialized add should succeed");
        remove_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("remove should finish after pane-count barrier release");
        add_thread.join().expect("add thread should not panic");
        remove_thread
            .join()
            .expect("remove thread should not panic");

        assert_eq!(&*observed.lock(), &["added", "removed"]);
        assert!(mux.get_pane(89).is_none());
        assert_eq!(kills.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn panicking_pane_kill_cannot_wedge_later_lifecycle_delivery() {
        let mux = Mux::new(None);
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observed_for_subscriber = Arc::clone(&observed);
        mux.subscribe(move |notification| {
            if let MuxNotification::PaneRemoved(pane_id) = notification {
                observed_for_subscriber.lock().push(pane_id);
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");

        let (panicking, _) = KillCountingPane::new_with_kill_callback(89, test_size(), || {
            panic!("intentional pane kill panic");
        });
        let ordinary = register_test_pane(&mux, 90);
        mux.add_pane(&panicking)
            .expect("panicking test pane should register");

        mux.remove_pane_if_same(89, &panicking);
        mux.remove_pane_if_same(90, &ordinary);

        assert_eq!(
            &*observed.lock(),
            &[89, 90],
            "a panic must not leave an unready lifecycle ticket at the queue head",
        );
        assert!(mux.pending_pane_lifecycle.lock().notifications.is_empty());
    }

    #[test]
    fn same_id_replacement_is_fenced_through_removed_callback() {
        let mux = Arc::new(Mux::new(None));
        let replacement_attempted = Arc::new(AtomicBool::new(false));
        let replacement_rejected = Arc::new(AtomicBool::new(false));
        let replacement_attempted_from_kill = Arc::clone(&replacement_attempted);
        let replacement_rejected_from_kill = Arc::clone(&replacement_rejected);
        let replacement = KillCountingPane::new(91, test_size()).0;
        let replacement_from_kill = Arc::clone(&replacement);
        let mux_from_kill = Arc::clone(&mux);
        let (original, _) = KillCountingPane::new_with_kill_callback(91, test_size(), move || {
            replacement_attempted_from_kill.store(true, Ordering::SeqCst);
            replacement_rejected_from_kill.store(
                mux_from_kill.add_pane(&replacement_from_kill).is_err(),
                Ordering::SeqCst,
            );
        });
        mux.add_pane(&original)
            .expect("original test pane should register");

        let replacement_for_subscriber = Arc::clone(&replacement);
        let mux_for_subscriber = Arc::clone(&mux);
        mux.subscribe(move |notification| {
            if let MuxNotification::PaneRemoved(91) = notification {
                assert!(
                    mux_for_subscriber.get_pane(91).is_none(),
                    "PaneRemoved must not expose a same-id replacement",
                );
                assert!(
                    mux_for_subscriber
                        .add_pane(&replacement_for_subscriber)
                        .is_err(),
                    "the retiring ID must remain fenced through subscriber callbacks",
                );
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");

        mux.remove_pane_if_same(91, &original);

        assert!(replacement_attempted.load(Ordering::SeqCst));
        assert!(replacement_rejected.load(Ordering::SeqCst));
        mux.add_pane(&replacement)
            .expect("same ID may register after the removal callback completes");
        assert!(mux
            .get_pane(91)
            .is_some_and(|pane| Arc::ptr_eq(&pane, &replacement)));
    }

    #[test]
    fn reentrant_removal_keeps_same_id_fenced_until_queued_removed_dispatch() {
        let mux = Arc::new(Mux::new(None));
        let (original, _) = KillCountingPane::new(92, test_size());
        let (replacement, _) = KillCountingPane::new(92, test_size());
        let first_add = Arc::new(AtomicBool::new(true));
        let replacement_rejected = Arc::new(AtomicBool::new(false));
        let observed = Arc::new(Mutex::new(Vec::new()));

        let mux_for_subscriber = Arc::clone(&mux);
        let original_for_subscriber = Arc::clone(&original);
        let replacement_for_subscriber = Arc::clone(&replacement);
        let first_add_for_subscriber = Arc::clone(&first_add);
        let rejected_for_subscriber = Arc::clone(&replacement_rejected);
        let observed_for_subscriber = Arc::clone(&observed);
        mux.subscribe(move |notification| {
            match notification {
                MuxNotification::PaneAdded(92)
                    if first_add_for_subscriber.swap(false, Ordering::SeqCst) =>
                {
                    observed_for_subscriber.lock().push("added");
                    mux_for_subscriber.remove_pane_if_same(92, &original_for_subscriber);
                    rejected_for_subscriber.store(
                        mux_for_subscriber
                            .add_pane(&replacement_for_subscriber)
                            .is_err(),
                        Ordering::SeqCst,
                    );
                }
                MuxNotification::PaneRemoved(92) => {
                    observed_for_subscriber.lock().push("removed");
                }
                _ => {}
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");

        mux.add_pane(&original)
            .expect("original pane should publish before reentrant removal");

        assert!(
            replacement_rejected.load(Ordering::SeqCst),
            "completion during an active drain must not release the retiring ID early",
        );
        assert_eq!(&*observed.lock(), &["added", "removed"]);
        mux.add_pane(&replacement)
            .expect("replacement may register after queued PaneRemoved dispatch");
    }

    #[test]
    fn duplicate_unqualified_removal_cannot_release_an_inflight_fence() {
        let mux = Arc::new(Mux::new(None));
        let replacement: Arc<dyn Pane> = KillCountingPane::new(92, test_size()).0;
        let replacement_rejected = Arc::new(AtomicBool::new(false));
        let mux_from_kill = Arc::clone(&mux);
        let replacement_from_kill = Arc::clone(&replacement);
        let rejected_from_kill = Arc::clone(&replacement_rejected);
        let (original, _) = KillCountingPane::new_with_kill_callback(92, test_size(), move || {
            // This stale duplicate removal does not own the retirement
            // fence established by the outer removal.
            mux_from_kill.remove_pane(92);
            rejected_from_kill.store(
                mux_from_kill.add_pane(&replacement_from_kill).is_err(),
                Ordering::SeqCst,
            );
        });
        let original: Arc<dyn Pane> = original;
        mux.add_pane(&original).expect("register original pane");

        mux.remove_pane(92);

        assert!(
            replacement_rejected.load(Ordering::SeqCst),
            "a duplicate cleanup-only removal must not clear another removal's fence",
        );
        mux.add_pane(&replacement)
            .expect("fence should release after the authoritative Removed callback");
    }

    #[test]
    fn tab_removal_fences_ids_during_kill_and_removed_callbacks() {
        let mux = Arc::new(Mux::new(None));
        let (normal_replacement, _) = KillCountingPane::new(93, test_size());
        let (local_replacement, _) = KillCountingPane::new(94, test_size());
        let kill_rejected = Arc::new(AtomicBool::new(false));
        let normal_callback_rejected = Arc::new(AtomicBool::new(false));
        let local_callback_rejected = Arc::new(AtomicBool::new(false));

        let mux_for_kill = Arc::clone(&mux);
        let replacement_for_kill = Arc::clone(&normal_replacement);
        let kill_rejected_from_callback = Arc::clone(&kill_rejected);
        let (normal_pane, _) =
            KillCountingPane::new_with_kill_callback(93, test_size(), move || {
                kill_rejected_from_callback.store(
                    mux_for_kill.add_pane(&replacement_for_kill).is_err(),
                    Ordering::SeqCst,
                );
            });
        let normal_tab = Arc::new(Tab::new(&test_size()));
        normal_tab.assign_pane(&normal_pane);
        mux.add_tab_and_active_pane(&normal_tab)
            .expect("normal tab should register");

        let (local_pane, _) = KillCountingPane::new(94, test_size());
        let local_tab = Arc::new(Tab::new(&test_size()));
        local_tab.assign_pane(&local_pane);
        mux.add_tab_and_active_pane(&local_tab)
            .expect("local-only tab should register");

        let mux_for_subscriber = Arc::clone(&mux);
        let normal_replacement_for_subscriber = Arc::clone(&normal_replacement);
        let local_replacement_for_subscriber = Arc::clone(&local_replacement);
        let normal_rejected_for_subscriber = Arc::clone(&normal_callback_rejected);
        let local_rejected_for_subscriber = Arc::clone(&local_callback_rejected);
        mux.subscribe(move |notification| {
            match notification {
                MuxNotification::PaneRemoved(93) => {
                    normal_rejected_for_subscriber.store(
                        mux_for_subscriber
                            .add_pane(&normal_replacement_for_subscriber)
                            .is_err(),
                        Ordering::SeqCst,
                    );
                }
                MuxNotification::PaneRemoved(94) => {
                    local_rejected_for_subscriber.store(
                        mux_for_subscriber
                            .add_pane(&local_replacement_for_subscriber)
                            .is_err(),
                        Ordering::SeqCst,
                    );
                }
                _ => {}
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");

        mux.remove_tab(normal_tab.tab_id())
            .expect("normal tab should be removed");
        mux.remove_tab_local_only(local_tab.tab_id())
            .expect("local-only tab should be removed");

        assert!(kill_rejected.load(Ordering::SeqCst));
        assert!(normal_callback_rejected.load(Ordering::SeqCst));
        assert!(local_callback_rejected.load(Ordering::SeqCst));
        mux.add_pane(&normal_replacement)
            .expect("normal replacement may register after PaneRemoved");
        mux.add_pane(&local_replacement)
            .expect("local replacement may register after PaneRemoved");
    }

    #[test]
    fn tab_removal_during_active_pane_preparation_fences_topology_publication() {
        let mux = Arc::new(Mux::new(None));
        let tab = Arc::new(Tab::new(&test_size()));
        let tab_id = tab.tab_id();
        let (pane, kills, reader_entered, release_reader) = pane_with_blocked_reader(88);
        tab.assign_pane(&pane);
        mux.add_tab_no_panes(&tab)
            .expect("test tab should be provisionally registered");
        let mux_for_add = Arc::clone(&mux);
        let tab_for_add = Arc::clone(&tab);
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let add_thread = std::thread::spawn(move || {
            let result = mux_for_add.add_tab_and_active_pane(&tab_for_add);
            result_tx
                .send(result)
                .expect("test should still be waiting for registration result");
        });

        reader_entered
            .recv_timeout(Duration::from_secs(30))
            .expect("pane preparation should reach its blocking reader callback");
        let removed_tab = mux
            .remove_tab(tab_id)
            .expect("exact provisionally registered tab should be removed");
        assert!(Arc::ptr_eq(&removed_tab, &tab));
        release_reader
            .send(())
            .expect("blocked reader callback should still be waiting");

        let err = result_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("cancelled tab registration should finish")
            .expect_err("a cancelled active pane must prevent tab publication");
        assert_eq!(
            err.downcast_ref::<PanePreparationCancelled>(),
            Some(&PanePreparationCancelled { pane_id: 88 })
        );
        add_thread
            .join()
            .expect("registration thread should not panic");
        assert!(mux.get_pane(88).is_none());
        assert!(mux.get_tab(tab_id).is_none());
        assert_eq!(kills.load(Ordering::SeqCst), 0);
        assert!(mux.pane_preparations.lock().is_empty());
    }

    #[test]
    fn add_tab_does_not_publish_topology_when_its_pane_id_collides() {
        let mux = Mux::new(None);
        let first_tab = Arc::new(Tab::new(&test_size()));
        let (first, _) = KillCountingPane::new(78, test_size());
        first_tab.assign_pane(&first);
        mux.add_tab_and_active_pane(&first_tab)
            .expect("first tab and pane should register");

        let duplicate_tab = Arc::new(Tab::new(&test_size()));
        let duplicate_tab_id = duplicate_tab.tab_id();
        let (duplicate, _) = KillCountingPane::new(78, test_size());
        duplicate_tab.assign_pane(&duplicate);

        let err = mux
            .add_tab_and_active_pane(&duplicate_tab)
            .expect_err("colliding pane must reject its containing tab");
        assert!(err.downcast_ref::<PaneIdCollision>().is_some());
        assert!(
            mux.get_tab(duplicate_tab_id).is_none(),
            "a tab containing the rejected pane must not become observable"
        );
        let registered = mux
            .get_pane(78)
            .expect("first pane should remain registered");
        assert!(Arc::ptr_eq(&registered, &first));
    }

    #[test]
    fn add_tab_no_panes_rejects_a_different_instance_with_the_same_id() {
        let mux = Mux::new(None);
        let first = Arc::new(Tab::new(&test_size()));
        let duplicate = Arc::new(Tab::new(&test_size()));
        let colliding_id = duplicate.tab_id();
        mux.tabs.write().insert(colliding_id, Arc::clone(&first));

        let err = mux
            .add_tab_no_panes(&duplicate)
            .expect_err("a different tab instance must not replace the registered tab");
        assert!(err
            .to_string()
            .contains("already registered to a different tab instance"));
        let registered = mux
            .get_tab(colliding_id)
            .expect("first tab should remain registered");
        assert!(Arc::ptr_eq(&registered, &first));
    }

    #[test]
    fn add_tab_does_not_publish_topology_when_reader_acquisition_fails() {
        let mux = Mux::new(None);
        let tab = Arc::new(Tab::new(&test_size()));
        let tab_id = tab.tab_id();
        let (pane, _) = KillCountingPane::new_with_reader(79, test_size(), None, true);
        tab.assign_pane(&pane);

        let err = mux
            .add_tab_and_active_pane(&tab)
            .expect_err("reader acquisition failure must reject the tab and pane");
        assert!(err
            .to_string()
            .contains("intentional test pane reader acquisition failure"));
        assert!(mux.get_pane(79).is_none());
        assert!(mux.get_tab(tab_id).is_none());
    }

    #[test]
    fn add_tab_starts_reader_only_after_topology_and_pane_added_are_visible() {
        let mux = Arc::new(Mux::new(None));
        let tab = Arc::new(Tab::new(&test_size()));
        let tab_id = tab.tab_id();
        let pane_added = Arc::new(AtomicBool::new(false));
        let pane_added_for_subscriber = Arc::clone(&pane_added);
        mux.subscribe(move |notification| {
            if matches!(notification, MuxNotification::PaneAdded(80)) {
                pane_added_for_subscriber.store(true, Ordering::SeqCst);
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let reader = RegistrationObservingReader {
            mux: Arc::clone(&mux),
            pane_id: 80,
            tab_id,
            pane_added,
            result_tx: Some(result_tx),
        };
        let (pane, _) =
            KillCountingPane::new_with_reader(80, test_size(), Some(Box::new(reader)), false);
        tab.assign_pane(&pane);

        mux.add_tab_and_active_pane(&tab)
            .expect("tab, pane, and PaneAdded should publish before reader start");
        let (pane_was_registered, tab_contained_pane, pane_added_was_emitted) = result_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("pane reader should report its initial publication state");
        assert!(pane_was_registered);
        assert!(tab_contained_pane);
        assert!(pane_added_was_emitted);
    }

    #[test]
    fn dropping_unreleased_reader_gate_exits_without_touching_pane_or_reading() {
        let mux = Mux::new(None);
        let reads = Arc::new(AtomicUsize::new(0));
        let (dropped_tx, dropped_rx) = std::sync::mpsc::channel();
        let reader = CancellationObservingReader {
            reads: Arc::clone(&reads),
            dropped_tx: Some(dropped_tx),
        };
        let (pane, _) =
            KillCountingPane::new_with_reader(81, test_size(), Some(Box::new(reader)), false);

        let reader_start_gate = {
            let _preparation_claim = mux
                .claim_pane_preparation(&pane)
                .expect("pane preparation claim should succeed")
                .expect("new pane should require a preparation claim");
            let prepared = mux
                .prepare_claimed_pane_registration(&pane, pane.pane_id())
                .expect("pane preparation should succeed");
            mux.spawn_prepared_pane_reader(&pane, prepared)
                .expect("reader thread should spawn")
                .expect("pane reader should produce a start gate")
        };

        assert!(
            mux.get_pane(81).is_none(),
            "spawning a gated reader must not publish the pane"
        );
        drop(reader_start_gate);
        dropped_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("cancelled reader thread should drop its unread reader");
        assert_eq!(reads.load(Ordering::SeqCst), 0);
        assert!(mux.get_pane(81).is_none());
    }

    #[test]
    fn pane_reader_callback_can_reenter_registration_for_an_unrelated_pane() {
        let mux = Arc::new(Mux::new(None));
        let (nested, _) = KillCountingPane::new(83, test_size());
        let mux_for_reader = Arc::clone(&mux);
        let nested_for_reader = Arc::clone(&nested);
        let (outer, _) = KillCountingPane::new_with_reader_callback(82, test_size(), move || {
            assert!(
                mux_for_reader.pane_registration.try_lock().is_some(),
                "external Pane callbacks must not run under the publication mutex"
            );
            mux_for_reader
                .add_pane(&nested_for_reader)
                .expect("reader callback should reenter registration");
        });

        mux.add_pane(&outer)
            .expect("outer pane should register after its callback returns");

        let registered_outer = mux.get_pane(82).expect("outer pane should be registered");
        let registered_nested = mux.get_pane(83).expect("nested pane should be registered");
        assert!(Arc::ptr_eq(&registered_outer, &outer));
        assert!(Arc::ptr_eq(&registered_nested, &nested));
    }

    #[test]
    fn pane_kill_callback_can_reenter_registration_without_registry_lock() {
        let mux = Arc::new(Mux::new(None));
        let (nested, _) = KillCountingPane::new(85, test_size());
        let mux_for_kill = Arc::clone(&mux);
        let nested_for_kill = Arc::clone(&nested);
        let (removed, kills) =
            KillCountingPane::new_with_kill_callback(84, test_size(), move || {
                assert!(
                    mux_for_kill.panes.try_write().is_some(),
                    "Pane::kill must not run under the pane registry write lock"
                );
                mux_for_kill
                    .add_pane(&nested_for_kill)
                    .expect("kill callback should reenter registration");
            });
        mux.add_pane(&removed)
            .expect("pane under test should register");

        mux.remove_pane(84);

        assert_eq!(kills.load(Ordering::SeqCst), 1);
        assert!(mux.get_pane(84).is_none());
        let registered_nested = mux.get_pane(85).expect("nested pane should be registered");
        assert!(Arc::ptr_eq(&registered_nested, &nested));
    }

    #[test]
    fn pane_registration_rollback_never_removes_a_replacement_instance() {
        let mux = Mux::new(None);
        let (failed_registration, _) = KillCountingPane::new(91, test_size());
        let (replacement, _) = KillCountingPane::new(91, test_size());
        mux.panes.write().insert(91, Arc::clone(&replacement));

        assert!(!mux.remove_pane_registration_if_same(91, &failed_registration));
        let registered = mux
            .get_pane(91)
            .expect("pointer-mismatched rollback must preserve replacement pane");
        assert!(Arc::ptr_eq(&registered, &replacement));

        assert!(mux.remove_pane_registration_if_same(91, &replacement));
        assert!(mux.get_pane(91).is_none());

        let failed_tab = Arc::new(Tab::new(&test_size()));
        let replacement_tab = Arc::new(Tab::new(&test_size()));
        let replacement_tab_id = replacement_tab.tab_id();
        mux.tabs
            .write()
            .insert(replacement_tab_id, Arc::clone(&replacement_tab));

        assert!(!mux.remove_tab_registration_if_same(replacement_tab_id, &failed_tab));
        let registered_tab = mux
            .get_tab(replacement_tab_id)
            .expect("pointer-mismatched rollback must preserve replacement tab");
        assert!(Arc::ptr_eq(&registered_tab, &replacement_tab));

        assert!(mux.remove_tab_registration_if_same(replacement_tab_id, &replacement_tab));
        assert!(mux.get_tab(replacement_tab_id).is_none());
    }

    #[test]
    fn default_workspace_value() {
        assert_eq!(DEFAULT_WORKSPACE, "default");
    }

    #[test]
    fn synchronized_output_decrqm_response_reports_hold_state() {
        assert_eq!(synchronized_output_decrqm_response(true), b"\x1b[?2026;1$y");
        assert_eq!(
            synchronized_output_decrqm_response(false),
            b"\x1b[?2026;2$y"
        );
    }

    #[test]
    fn synchronized_output_query_is_answered_from_parser_hold_state() {
        let mut parser = termwiz::escape::parser::Parser::new();
        let mut hold = SynchronizedOutputHold::default();
        let mut responses = Vec::new();
        let mut forwarded_actions = Vec::new();
        let mut events = Vec::new();

        parser.parse(
            b"\x1b[?2026h\x1b[?2026$p\x1b[?2026l\x1b[?2026$p",
            |action| {
                let effect = handle_synchronized_output_action(&action, &mut hold, |hold| {
                    responses.push(synchronized_output_decrqm_response(hold).to_vec());
                });
                if let Some(outcome) = effect.depth_outcome {
                    events.push(SynchronizedOutputEvent::Depth {
                        outcome,
                        max_depth: hold.max_depth(),
                    });
                }
                if effect.handled {
                    events.push(SynchronizedOutputEvent::ModeQuery);
                }
                if !effect.handled {
                    forwarded_actions.push(action);
                }
            },
        );

        assert_eq!(
            responses,
            vec![b"\x1b[?2026;1$y".to_vec(), b"\x1b[?2026;2$y".to_vec()]
        );
        assert_eq!(
            forwarded_actions.len(),
            2,
            "mode-query actions must be answered directly, not forwarded into the held action buffer",
        );
        assert_eq!(
            events,
            vec![
                SynchronizedOutputEvent::Depth {
                    outcome: SynchronizedOutputDepthOutcome::Opened { new_depth: 1 },
                    max_depth: 1,
                },
                SynchronizedOutputEvent::ModeQuery,
                SynchronizedOutputEvent::Depth {
                    outcome: SynchronizedOutputDepthOutcome::Flushed,
                    max_depth: 1,
                },
                SynchronizedOutputEvent::ModeQuery,
            ],
        );
        assert!(matches!(
            &forwarded_actions[0],
            Action::CSI(CSI::Mode(Mode::SetDecPrivateMode(DecPrivateMode::Code(
                DecPrivateModeCode::SynchronizedOutput
            ))))
        ));
        assert!(matches!(
            &forwarded_actions[1],
            Action::CSI(CSI::Mode(Mode::ResetDecPrivateMode(DecPrivateMode::Code(
                DecPrivateModeCode::SynchronizedOutput
            ))))
        ));
    }

    #[test]
    fn synchronized_output_hold_tracks_nested_depth_and_underflow() {
        let mut parser = termwiz::escape::parser::Parser::new();
        let mut hold = SynchronizedOutputHold::default();
        let mut outcomes = Vec::new();
        let mut flushes = 0;

        parser.parse(
            b"\x1b[?2026h\x1b[?2026h\x1b[?2026l\x1b[?2026l\x1b[?2026l",
            |action| {
                let effect = handle_synchronized_output_action(&action, &mut hold, |_| {});
                if effect.flush {
                    flushes += 1;
                }
                if let Some(outcome) = effect.depth_outcome {
                    outcomes.push(outcome);
                }
            },
        );

        assert_eq!(
            outcomes,
            vec![
                SynchronizedOutputDepthOutcome::Opened { new_depth: 1 },
                SynchronizedOutputDepthOutcome::Opened { new_depth: 2 },
                SynchronizedOutputDepthOutcome::Closed { new_depth: 1 },
                SynchronizedOutputDepthOutcome::Flushed,
                SynchronizedOutputDepthOutcome::Underflow,
            ]
        );
        assert_eq!(flushes, 1, "only the ESU that closes depth to zero flushes");
        assert_eq!(hold.max_depth(), 2);
        assert!(!hold.is_holding());
    }

    #[test]
    fn synchronized_output_soft_reset_flushes_without_operator_attribution() {
        let mut parser = termwiz::escape::parser::Parser::new();
        let mut hold = SynchronizedOutputHold::default();
        let mut flushes = 0;

        parser.parse(b"\x1b[?2026h\x1b[!p", |action| {
            let effect = handle_synchronized_output_action(&action, &mut hold, |_| {});
            if effect.flush {
                flushes += 1;
            }
        });

        assert_eq!(flushes, 1);
        assert!(!hold.is_holding());
    }

    #[derive(Debug, Clone, Copy)]
    enum SynchronizedOutputWireOp {
        Set,
        Reset,
        Query,
    }

    fn synchronized_output_wire_op_strategy() -> impl Strategy<Value = SynchronizedOutputWireOp> {
        prop_oneof![
            Just(SynchronizedOutputWireOp::Set),
            Just(SynchronizedOutputWireOp::Reset),
            Just(SynchronizedOutputWireOp::Query),
        ]
    }

    fn append_synchronized_output_wire_op(bytes: &mut Vec<u8>, op: SynchronizedOutputWireOp) {
        match op {
            SynchronizedOutputWireOp::Set => bytes.extend_from_slice(b"\x1b[?2026h"),
            SynchronizedOutputWireOp::Reset => bytes.extend_from_slice(b"\x1b[?2026l"),
            SynchronizedOutputWireOp::Query => bytes.extend_from_slice(b"\x1b[?2026$p"),
        }
    }

    proptest! {
        #[test]
        fn synchronized_output_escape_stream_queries_follow_hold_state(
            ops in proptest::collection::vec(synchronized_output_wire_op_strategy(), 1..64),
            chunk_sizes in proptest::collection::vec(1usize..8, 1..128),
        ) {
            let mut expected_depth = 0_u32;
            let mut expected_responses = Vec::new();
            let mut expected_forwarded = 0usize;
            let mut input = Vec::new();

            for op in &ops {
                append_synchronized_output_wire_op(&mut input, *op);
                match op {
                    SynchronizedOutputWireOp::Set => {
                        expected_depth = expected_depth.saturating_add(1);
                        expected_forwarded += 1;
                    }
                    SynchronizedOutputWireOp::Reset => {
                        expected_depth = expected_depth.saturating_sub(1);
                        expected_forwarded += 1;
                    }
                    SynchronizedOutputWireOp::Query => {
                        expected_responses
                            .push(synchronized_output_decrqm_response(expected_depth > 0).to_vec());
                    }
                }
            }

            let mut parser = termwiz::escape::parser::Parser::new();
            let mut hold = SynchronizedOutputHold::default();
            let mut responses = Vec::new();
            let mut forwarded = 0usize;
            let mut offset = 0usize;
            let mut chunk_iter = chunk_sizes.iter().copied().cycle();

            while offset < input.len() {
                let chunk_len = chunk_iter.next().unwrap_or(input.len()).min(input.len() - offset);
                parser.parse(&input[offset..offset + chunk_len], |action| {
                    let effect = handle_synchronized_output_action(&action, &mut hold, |hold| {
                        responses.push(synchronized_output_decrqm_response(hold).to_vec());
                    });
                    if !effect.handled {
                        forwarded += 1;
                    }
                });
                offset += chunk_len;
            }

            prop_assert_eq!(responses, expected_responses);
            prop_assert_eq!(forwarded, expected_forwarded);
            prop_assert_eq!(hold.is_holding(), expected_depth > 0);
        }
    }

    #[test]
    fn mux_notification_pane_output_debug() {
        let n = MuxNotification::PaneOutput(42);
        let dbg = format!("{:?}", n);
        assert!(dbg.contains("PaneOutput"));
        assert!(dbg.contains("42"));
    }

    #[test]
    fn mux_notification_synchronized_output_debug_and_clone() {
        let n = MuxNotification::SynchronizedOutput {
            pane_id: 7,
            event: SynchronizedOutputEvent::Drain {
                cause: SynchronizedOutputDrainCause::Watchdog,
                bytes: 8192,
                depth_outcome: None,
                max_depth: 3,
            },
        };
        let dbg = format!("{:?}", n.clone());
        assert!(dbg.contains("SynchronizedOutput"));
        assert!(dbg.contains("Watchdog"));
        assert!(dbg.contains("7"));
    }

    #[test]
    fn mux_notification_pane_added_clone() {
        let n = MuxNotification::PaneAdded(1);
        let n2 = n.clone();
        let dbg = format!("{:?}", n2);
        assert!(dbg.contains("PaneAdded"));
    }

    #[test]
    fn mux_notification_pane_removed() {
        let n = MuxNotification::PaneRemoved(5);
        let dbg = format!("{:?}", n);
        assert!(dbg.contains("PaneRemoved"));
    }

    #[test]
    fn mux_notification_window_created() {
        let n = MuxNotification::WindowCreated(0);
        let dbg = format!("{:?}", n);
        assert!(dbg.contains("WindowCreated"));
    }

    #[test]
    fn mux_notification_window_removed() {
        let n = MuxNotification::WindowRemoved(1);
        let dbg = format!("{:?}", n);
        assert!(dbg.contains("WindowRemoved"));
    }

    #[test]
    fn mux_notification_empty() {
        let n = MuxNotification::Empty;
        let dbg = format!("{:?}", n);
        assert!(dbg.contains("Empty"));
    }

    #[test]
    fn subscribe_handle_can_unsubscribe() {
        let mux = Mux::new(None);
        let notifications = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&notifications);

        let sub_id = mux
            .subscribe(move |_| {
                observed.fetch_add(1, Ordering::Relaxed);
                true
            })
            .expect("test mux subscription should allocate an identifier");

        mux.notify(MuxNotification::Empty);
        assert_eq!(notifications.load(Ordering::Relaxed), 1);

        assert!(mux.unsubscribe(sub_id));
        mux.notify(MuxNotification::Empty);
        assert_eq!(notifications.load(Ordering::Relaxed), 1);
        assert!(!mux.unsubscribe(sub_id));
    }

    #[test]
    fn legacy_saturating_allocator_repeats_the_terminal_value() {
        let counter = AtomicUsize::new(usize::MAX);

        assert_eq!(next_saturating_usize_id(&counter), usize::MAX);
        assert_eq!(next_saturating_usize_id(&counter), usize::MAX);
        assert_eq!(counter.load(Ordering::Relaxed), usize::MAX);
    }

    #[test]
    fn checked_id_reservation_uses_terminal_value_only_as_exhausted_sentinel() {
        let counter = AtomicUsize::new(usize::MAX - 2);

        assert_eq!(
            try_reserve_usize_ids(&counter, 2, "test").unwrap(),
            usize::MAX - 2..usize::MAX
        );
        assert_eq!(counter.load(Ordering::Relaxed), usize::MAX);

        let err = try_reserve_usize_ids(&counter, 1, "test").unwrap_err();
        assert_eq!(err.namespace(), "test");
        assert_eq!(err.requested(), 1);
        assert_eq!(counter.load(Ordering::Relaxed), usize::MAX);
    }

    #[test]
    fn checked_id_reservation_is_atomic_when_the_requested_range_will_not_fit() {
        let counter = AtomicUsize::new(usize::MAX - 1);

        let err = try_reserve_usize_ids(&counter, 2, "test").unwrap_err();
        assert_eq!(err.namespace(), "test");
        assert_eq!(err.requested(), 2);
        assert!(err
            .to_string()
            .contains("insufficient remaining capacity for a reservation of 2"));
        assert_eq!(counter.load(Ordering::Relaxed), usize::MAX - 1);
        assert_eq!(
            try_reserve_usize_ids(&counter, 1, "test").unwrap(),
            usize::MAX - 1..usize::MAX
        );
    }

    #[test]
    fn concurrent_checked_id_reservations_are_unique_and_gap_free() {
        const THREADS: usize = 8;
        const RESERVATIONS_PER_THREAD: usize = 64;
        const IDS_PER_RESERVATION: usize = 7;

        let counter = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(std::sync::Barrier::new(THREADS));
        let observed = Arc::new(Mutex::new(Vec::new()));
        let mut workers = Vec::new();

        for _ in 0..THREADS {
            let counter = Arc::clone(&counter);
            let barrier = Arc::clone(&barrier);
            let observed = Arc::clone(&observed);
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..RESERVATIONS_PER_THREAD {
                    let reserved =
                        try_reserve_usize_ids(&counter, IDS_PER_RESERVATION, "test").unwrap();
                    observed.lock().extend(reserved);
                }
            }));
        }

        for worker in workers {
            worker.join().expect("reservation worker should not panic");
        }

        let expected = THREADS * RESERVATIONS_PER_THREAD * IDS_PER_RESERVATION;
        let mut observed = observed.lock();
        observed.sort_unstable();
        assert_eq!(&*observed, &(0..expected).collect::<Vec<_>>());
        assert_eq!(counter.load(Ordering::Relaxed), expected);
    }

    #[test]
    fn notification_callbacks_can_unsubscribe_without_lock_reentrancy() {
        let mux = Arc::new(Mux::new(None));
        let first_notifications = Arc::new(AtomicUsize::new(0));
        let second_notifications = Arc::new(AtomicUsize::new(0));
        let second_sub_id = Arc::new(Mutex::new(None));

        let mux_for_first = Arc::clone(&mux);
        let second_sub_id_for_first = Arc::clone(&second_sub_id);
        let first_notifications_for_first = Arc::clone(&first_notifications);
        mux.subscribe(move |_| {
            first_notifications_for_first.fetch_add(1, Ordering::Relaxed);
            if let Some(sub_id) = *second_sub_id_for_first.lock() {
                mux_for_first.unsubscribe(sub_id);
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");

        let second_notifications_for_second = Arc::clone(&second_notifications);
        let second_id = mux
            .subscribe(move |_| {
                second_notifications_for_second.fetch_add(1, Ordering::Relaxed);
                true
            })
            .expect("test mux subscription should allocate an identifier");
        *second_sub_id.lock() = Some(second_id);

        mux.dispatch_notification(MuxNotification::Empty);
        assert_eq!(first_notifications.load(Ordering::Relaxed), 1);
        assert_eq!(
            second_notifications.load(Ordering::Relaxed),
            1,
            "snapshot fanout allows already-snapshotted subscribers to observe the current event",
        );

        mux.dispatch_notification(MuxNotification::Empty);
        assert_eq!(first_notifications.load(Ordering::Relaxed), 2);
        assert_eq!(
            second_notifications.load(Ordering::Relaxed),
            1,
            "unsubscribe during callback should remove the subscriber for future notifications",
        );
    }

    #[test]
    fn high_rate_alert_dedupe_preserves_value_bearing_progress_updates() {
        let mux = Mux::new(None);
        let notifications = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&notifications);
        mux.subscribe(move |notification| {
            if matches!(notification, MuxNotification::Alert { .. }) {
                observed.fetch_add(1, Ordering::Relaxed);
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");

        let cwd = MuxNotification::Alert {
            pane_id: 7,
            alert: frankenterm_term::Alert::CurrentWorkingDirectoryChanged,
        };
        mux.notify(cwd.clone());
        mux.notify(cwd.clone());
        assert_eq!(
            notifications.load(Ordering::Relaxed),
            1,
            "idempotent same-pane alerts should dedupe inside the frame window",
        );

        mux.notify(MuxNotification::Alert {
            pane_id: 7,
            alert: frankenterm_term::Alert::Progress(frankenterm_term::Progress::Percentage(42)),
        });
        mux.notify(MuxNotification::Alert {
            pane_id: 7,
            alert: frankenterm_term::Alert::Progress(frankenterm_term::Progress::Percentage(64)),
        });
        assert_eq!(
            notifications.load(Ordering::Relaxed),
            3,
            "newer value-bearing progress state must never be timer-dropped",
        );

        mux.notify(MuxNotification::Alert {
            pane_id: 7,
            alert: frankenterm_term::Alert::OutputSinceFocusLost,
        });
        assert_eq!(
            notifications.load(Ordering::Relaxed),
            4,
            "different idempotent alert kinds should not dedupe each other",
        );

        {
            let mut last = mux.last_high_rate_alert.lock();
            *last
                .get_mut(&(7, HighRateAlertKind::CurrentWorkingDirectoryChanged))
                .expect("first cwd alert should populate the dedupe map") = Instant::now()
                .checked_sub(HIGH_RATE_ALERT_DEDUPE_WINDOW + Duration::from_millis(1))
                .expect("test duration is small enough to subtract from now");
        }
        mux.notify(cwd);
        assert_eq!(
            notifications.load(Ordering::Relaxed),
            5,
            "same-pane same-kind alert should dispatch again after the dedupe window",
        );
    }

    #[test]
    fn remove_pane_discards_high_rate_alert_state() {
        let mux = Mux::new(None);
        mux.notify(MuxNotification::Alert {
            pane_id: 7,
            alert: frankenterm_term::Alert::OutputSinceFocusLost,
        });
        mux.notify(MuxNotification::Alert {
            pane_id: 7,
            alert: frankenterm_term::Alert::CurrentWorkingDirectoryChanged,
        });
        mux.notify(MuxNotification::Alert {
            pane_id: 8,
            alert: frankenterm_term::Alert::OutputSinceFocusLost,
        });

        {
            let last = mux.last_high_rate_alert.lock();
            assert!(last.contains_key(&(7, HighRateAlertKind::OutputSinceFocusLost)));
            assert!(last.contains_key(&(7, HighRateAlertKind::CurrentWorkingDirectoryChanged)));
            assert!(last.contains_key(&(8, HighRateAlertKind::OutputSinceFocusLost)));
        }

        mux.remove_pane(7);

        let last = mux.last_high_rate_alert.lock();
        assert!(
            !last.keys().any(|(pane_id, _)| *pane_id == 7),
            "remove_pane must not leave high-rate alert dedupe entries for a dead pane",
        );
        assert!(
            last.contains_key(&(8, HighRateAlertKind::OutputSinceFocusLost)),
            "tearing down one pane must not clear dedupe state for unrelated live panes",
        );
    }

    #[test]
    fn remove_pane_discards_client_focus_for_removed_pane() {
        let mux = Mux::new(None);
        let removed_client = Arc::new(ClientId::new());
        let unrelated_client = Arc::new(ClientId::new());
        mux.register_client(Arc::clone(&removed_client));
        mux.register_client(Arc::clone(&unrelated_client));
        mux.record_focus_for_client(&removed_client, 7);
        mux.record_focus_for_client(&unrelated_client, 8);

        {
            let clients = mux.clients.read();
            assert_eq!(clients[removed_client.as_ref()].focused_pane_id, Some(7));
            assert_eq!(clients[unrelated_client.as_ref()].focused_pane_id, Some(8));
        }

        mux.remove_pane(7);

        let clients = mux.clients.read();
        assert_eq!(
            clients[removed_client.as_ref()].focused_pane_id,
            None,
            "remove_pane must clear per-client focus state for the removed pane",
        );
        assert_eq!(
            clients[unrelated_client.as_ref()].focused_pane_id,
            Some(8),
            "removing one pane must not clear client focus for unrelated panes",
        );
    }

    #[test]
    fn unregister_client_discards_removed_active_identity() {
        let mux = Mux::new(None);
        let removed_client = Arc::new(ClientId::new());
        let retained_client = Arc::new(ClientId::new());
        mux.register_client(Arc::clone(&removed_client));
        mux.register_client(Arc::clone(&retained_client));

        mux.replace_identity(Some(Arc::clone(&retained_client)));
        mux.unregister_client(&removed_client);
        assert_eq!(
            mux.active_identity().as_deref(),
            Some(retained_client.as_ref()),
            "unregistering one client must not clear an unrelated active identity",
        );

        mux.unregister_client(&retained_client);
        assert_eq!(
            mux.active_identity(),
            None,
            "unregister_client must not leave a dead client id as the active identity",
        );
    }

    #[test]
    fn unregister_client_if_same_preserves_equal_replacement_instance() {
        let mux = Mux::new(None);
        let stale_client = Arc::new(ClientId::new());
        let replacement_client = Arc::new(stale_client.as_ref().clone());

        mux.register_client(Arc::clone(&stale_client));
        mux.register_client(Arc::clone(&replacement_client));

        assert!(
            !mux.unregister_client_if_same(&stale_client),
            "stale cleanup must not remove an equal-valued replacement registration",
        );
        assert!(
            mux.clients
                .read()
                .get(replacement_client.as_ref())
                .is_some_and(|info| Arc::ptr_eq(&info.client_id, &replacement_client)),
            "the replacement registration must survive stale cleanup",
        );

        mux.replace_identity(Some(Arc::clone(&replacement_client)));
        assert!(mux.unregister_client_if_same(&replacement_client));
        assert!(
            mux.active_identity().is_none(),
            "exact cleanup must clear the exact active identity",
        );
    }

    #[test]
    fn pane_output_without_scheduler_preserves_synchronous_notify_contract() {
        let mux = Mux::new(None);
        let pane = register_test_pane(&mux, 7);
        let pane_outputs = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&pane_outputs);
        mux.subscribe(move |notification| {
            if let MuxNotification::PaneOutput(pane_id) = notification {
                observed.lock().push(pane_id);
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");

        assert!(mux.enqueue_pane_output_notification_for_pane_with_scheduler_state(&pane, false));

        assert_eq!(&*pane_outputs.lock(), &[7]);
        assert!(mux.pending_pane_output.lock().notifications.is_empty());
        assert!(
            !mux.pane_output_drain_scheduled.load(Ordering::Relaxed),
            "synchronous fallback must leave no stranded drain lease",
        );
    }

    #[test]
    fn scheduled_pane_output_drain_remains_bound_to_originating_mux() {
        let _guard = global_test_lock();
        Mux::shutdown();
        let executor = promise::spawn::SimpleExecutor::new();
        let originating_mux = Arc::new(Mux::new(None));
        let replacement_mux = Arc::new(Mux::new(None));
        let _pane = register_test_pane(&originating_mux, 7);
        let pane_outputs = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&pane_outputs);
        originating_mux
            .subscribe(move |notification| {
                if let MuxNotification::PaneOutput(pane_id) = notification {
                    observed.lock().push(pane_id);
                }
                true
            })
            .expect("test mux subscription should allocate an identifier");

        Mux::set_mux(&originating_mux);
        originating_mux.enqueue_pane_output_notification(7);
        Mux::set_mux(&replacement_mux);
        executor
            .tick()
            .expect("scheduled pane-output drain should run");

        assert_eq!(
            &*pane_outputs.lock(),
            &[7],
            "mux replacement must not redirect an already-scheduled output drain",
        );
        assert!(originating_mux
            .pending_pane_output
            .lock()
            .notifications
            .is_empty());
        assert!(
            !originating_mux
                .pane_output_drain_scheduled
                .load(Ordering::Relaxed),
            "originating mux must not retain a permanently scheduled drain lease",
        );
        Mux::shutdown();
    }

    #[test]
    fn scheduled_pane_output_drain_does_not_retain_destroyed_mux() {
        let _guard = global_test_lock();
        Mux::shutdown();
        let executor = promise::spawn::SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
        let _pane = register_test_pane(&mux, 7);
        let pane_outputs = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&pane_outputs);
        mux.subscribe(move |notification| {
            if let MuxNotification::PaneOutput(pane_id) = notification {
                observed.lock().push(pane_id);
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");

        Mux::set_mux(&mux);
        mux.enqueue_pane_output_notification(7);
        Mux::shutdown();
        drop(mux);
        executor
            .tick()
            .expect("scheduled pane-output drain should run");

        assert!(
            pane_outputs.lock().is_empty(),
            "a deferred drain must not retain or notify a destroyed mux",
        );
    }

    #[test]
    fn pane_output_notifications_coalesce_until_flushed() {
        let _guard = global_test_lock();
        Mux::shutdown();
        let executor = promise::spawn::SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let _pane_7 = register_test_pane(&mux, 7);
        let _pane_8 = register_test_pane(&mux, 8);
        let pane_outputs = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&pane_outputs);
        mux.subscribe(move |notification| {
            if let MuxNotification::PaneOutput(pane_id) = notification {
                observed.lock().push(pane_id);
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");

        mux.enqueue_pane_output_notification(7);
        mux.enqueue_pane_output_notification(7);
        mux.enqueue_pane_output_notification(8);

        {
            let pending = mux.pending_pane_output.lock();
            assert_eq!(
                pending
                    .notifications
                    .iter()
                    .map(|notification| notification.pane_id)
                    .collect::<Vec<_>>(),
                vec![7, 8]
            );
            assert!(pending.queued.contains_key(&7));
            assert!(pending.queued.contains_key(&8));
        }
        assert!(pane_outputs.lock().is_empty());

        mux.flush_pending_pane_output_notifications();

        assert_eq!(&*pane_outputs.lock(), &[7, 8]);
        let pending = mux.pending_pane_output.lock();
        assert!(pending.notifications.is_empty());
        assert!(pending.queued.is_empty());
        assert!(
            !mux.pane_output_drain_scheduled.load(Ordering::Relaxed),
            "flush should clear the scheduled flag once the queue is empty",
        );
        drop(pending);

        executor
            .tick()
            .expect("scheduled pane-output drain should run");
        Mux::shutdown();
    }

    #[test]
    fn discarded_pane_output_notification_is_not_flushed() {
        let _guard = global_test_lock();
        Mux::shutdown();
        let executor = promise::spawn::SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let _pane_7 = register_test_pane(&mux, 7);
        let _pane_8 = register_test_pane(&mux, 8);
        let pane_outputs = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&pane_outputs);
        mux.subscribe(move |notification| {
            if let MuxNotification::PaneOutput(pane_id) = notification {
                observed.lock().push(pane_id);
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");

        mux.enqueue_pane_output_notification(7);
        mux.enqueue_pane_output_notification(8);
        mux.discard_pending_pane_output_notification(7);

        {
            let pending = mux.pending_pane_output.lock();
            assert_eq!(
                pending
                    .notifications
                    .iter()
                    .map(|notification| notification.pane_id)
                    .collect::<Vec<_>>(),
                vec![8]
            );
            assert!(!pending.queued.contains_key(&7));
            assert!(pending.queued.contains_key(&8));
        }

        mux.flush_pending_pane_output_notifications();

        assert_eq!(
            &*pane_outputs.lock(),
            &[8],
            "discarded pane-output notifications must not flush after pane removal",
        );

        executor
            .tick()
            .expect("scheduled pane-output drain should run");
        Mux::shutdown();
    }

    #[test]
    fn remove_pane_discards_pending_output_for_removed_pane() {
        let _guard = global_test_lock();
        Mux::shutdown();
        let executor = promise::spawn::SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let _pane = register_test_pane(&mux, 7);
        let pane_outputs = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&pane_outputs);
        mux.subscribe(move |notification| {
            if let MuxNotification::PaneOutput(pane_id) = notification {
                observed.lock().push(pane_id);
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");

        mux.enqueue_pane_output_notification(7);
        mux.remove_pane(7);

        {
            let pending = mux.pending_pane_output.lock();
            assert!(
                pending.notifications.is_empty(),
                "remove_pane must clear queued output for the removed pane",
            );
            assert!(
                pending.queued.is_empty(),
                "remove_pane must clear queued pane ids even when the pane is already absent",
            );
        }

        mux.flush_pending_pane_output_notifications();
        executor
            .tick()
            .expect("scheduled pane-output drain should run");
        assert!(
            pane_outputs.lock().is_empty(),
            "stale pending output for an absent pane must not be dispatched",
        );
        Mux::shutdown();
    }

    #[test]
    fn pane_output_reentrant_enqueue_is_drained_before_returning() {
        let _guard = global_test_lock();
        Mux::shutdown();
        let executor = promise::spawn::SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let _pane_7 = register_test_pane(&mux, 7);
        let _pane_8 = register_test_pane(&mux, 8);
        let pane_outputs = Arc::new(Mutex::new(Vec::new()));
        let reentered = Arc::new(AtomicBool::new(false));

        let mux_for_subscriber = Arc::clone(&mux);
        let pane_outputs_for_subscriber = Arc::clone(&pane_outputs);
        let reentered_for_subscriber = Arc::clone(&reentered);
        mux.subscribe(move |notification| {
            if let MuxNotification::PaneOutput(pane_id) = notification {
                pane_outputs_for_subscriber.lock().push(pane_id);
                if pane_id == 7 && !reentered_for_subscriber.swap(true, Ordering::Relaxed) {
                    mux_for_subscriber.enqueue_pane_output_notification(8);
                }
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");

        mux.enqueue_pane_output_notification(7);
        mux.flush_pending_pane_output_notifications();

        assert_eq!(
            &*pane_outputs.lock(),
            &[7, 8],
            "reentrant pane-output enqueue should be drained before the current flush returns",
        );
        let pending = mux.pending_pane_output.lock();
        assert!(pending.notifications.is_empty());
        assert!(pending.queued.is_empty());
        assert!(
            !mux.pane_output_drain_scheduled.load(Ordering::Relaxed),
            "flush should clear the scheduled flag after draining reentrant enqueues",
        );
        drop(pending);

        executor
            .tick()
            .expect("scheduled pane-output drain should run");
        Mux::shutdown();
    }

    #[test]
    fn pane_output_from_stale_same_id_instance_is_rejected() {
        let _guard = global_test_lock();
        Mux::shutdown();
        let executor = promise::spawn::SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let stale = register_test_pane(&mux, 7);
        let pane_outputs = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&pane_outputs);
        mux.subscribe(move |notification| {
            if let MuxNotification::PaneOutput(pane_id) = notification {
                observed.lock().push(pane_id);
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");

        mux.remove_pane_if_same(7, &stale);
        let replacement = register_test_pane(&mux, 7);
        assert!(
            !mux.enqueue_pane_output_notification_for_pane_with_scheduler_state(&stale, true),
            "an old reader must not attribute output to a same-id replacement",
        );
        assert!(
            mux.enqueue_pane_output_notification_for_pane_with_scheduler_state(&replacement, true),
            "the exact replacement instance should retain output authority",
        );
        mux.flush_pending_pane_output_notifications();

        assert_eq!(&*pane_outputs.lock(), &[7]);
        executor
            .tick()
            .expect("scheduled pane-output drain should run");
        Mux::shutdown();
    }

    #[test]
    fn removal_during_output_batch_cancels_later_exact_output() {
        let _guard = global_test_lock();
        Mux::shutdown();
        let executor = promise::spawn::SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let _pane_7 = register_test_pane(&mux, 7);
        let _pane_8 = register_test_pane(&mux, 8);
        let pane_outputs = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&pane_outputs);
        let mux_for_subscriber = Arc::clone(&mux);
        mux.subscribe(move |notification| {
            if let MuxNotification::PaneOutput(pane_id) = notification {
                observed.lock().push(pane_id);
                if pane_id == 7 {
                    mux_for_subscriber.remove_pane(8);
                }
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");

        mux.enqueue_pane_output_notification(7);
        mux.enqueue_pane_output_notification(8);
        mux.flush_pending_pane_output_notifications();

        assert_eq!(
            &*pane_outputs.lock(),
            &[7],
            "PaneRemoved(8) must not be followed by stale PaneOutput(8)",
        );
        assert!(mux.get_pane(8).is_none());
        executor
            .tick()
            .expect("scheduled pane-output drain should run");
        Mux::shutdown();
    }

    #[test]
    fn resolve_spawn_tab_domain_reports_missing_default_domain() {
        let mux = Mux::new(None);

        assert_eq!(
            mux.resolve_spawn_tab_domain(None, &SpawnTabDomain::DefaultDomain)
                .map(|domain| domain.domain_id())
                .map_err(|error| error.to_string()),
            Err("no default domain configured".to_string()),
        );
        assert_eq!(
            mux.resolve_spawn_tab_domain(None, &SpawnTabDomain::CurrentPaneDomain)
                .map(|domain| domain.domain_id())
                .map_err(|error| error.to_string()),
            Err("no default domain configured".to_string()),
        );
    }

    #[test]
    fn window_builder_drop_after_mux_shutdown_does_not_panic() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let window_builder = mux.new_empty_window(None, None);

        Mux::shutdown();
        drop(window_builder);
    }

    #[test]
    fn window_builder_non_main_drop_without_scheduler_notifies() {
        let _guard = global_test_lock();
        Mux::shutdown();
        if promise::spawn::is_scheduler_configured() {
            return;
        }

        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let seen = Arc::new(AtomicUsize::new(0));
        let seen_for_subscriber = Arc::clone(&seen);
        mux.subscribe(move |notification| {
            if matches!(notification, MuxNotification::WindowCreated(_)) {
                seen_for_subscriber.fetch_add(1, Ordering::SeqCst);
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");

        let mux_for_thread = Arc::clone(&mux);
        let handle = std::thread::spawn(move || {
            let window_builder = mux_for_thread.new_empty_window(None, None);
            drop(window_builder);
        });
        handle
            .join()
            .expect("window builder thread should not panic");

        assert_eq!(seen.load(Ordering::SeqCst), 1);
        Mux::shutdown();
    }

    #[test]
    fn new_empty_window_without_global_mux_uses_instance_workspace() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let mux = Arc::new(Mux::new(None));
        let window_builder = mux.new_empty_window(None, None);
        let window_id = *window_builder;

        {
            let mut window = mux
                .get_window_mut(window_id)
                .expect("new_empty_window should register the window");
            assert_eq!(window.get_workspace(), DEFAULT_WORKSPACE);
            window.set_workspace("workspace-without-global-mux");
            assert_eq!(window.get_workspace(), "workspace-without-global-mux");
        }

        drop(window_builder);
        Mux::shutdown();
    }

    #[test]
    fn window_invalidated_notification_runs_after_window_lock_released() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let executor = promise::spawn::SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);

        let window_builder = mux.new_empty_window(None, None);
        let window_id = *window_builder;
        let observed = Arc::new(AtomicBool::new(false));
        let observed_for_subscriber = Arc::clone(&observed);
        let mux_for_subscriber = Arc::clone(&mux);
        mux.subscribe(move |notification| {
            if let MuxNotification::WindowInvalidated(notified_window_id) = notification {
                assert_eq!(notified_window_id, window_id);
                assert!(mux_for_subscriber.get_window(notified_window_id).is_some());
                observed_for_subscriber.store(true, Ordering::Relaxed);
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");

        let size = frankenterm_term::TerminalSize {
            rows: 1,
            cols: 1,
            pixel_width: 1,
            pixel_height: 1,
            dpi: 96,
        };
        let tab = Arc::new(Tab::new(&size));
        mux.add_tab_no_panes(&tab)
            .expect("test tab should register");
        mux.add_tab_to_window(&tab, window_id)
            .expect("tab should be added to test window");

        assert!(!observed.load(Ordering::Relaxed));
        executor
            .tick()
            .expect("window invalidation should be scheduled");
        assert!(observed.load(Ordering::Relaxed));

        drop(window_builder);
        Mux::shutdown();
    }

    #[test]
    fn move_tab_between_windows_preserves_live_tab_and_pane() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let src_window = mux.new_empty_window(Some("winunify".to_string()), None);
        let src_window_id = *src_window;
        let dst_window = mux.new_empty_window(Some("winunify".to_string()), None);
        let dst_window_id = *dst_window;
        let (tab, kills) = tab_with_kill_counter(&mux, 401);
        let tab_id = tab.tab_id();
        mux.add_tab_to_window(&tab, src_window_id)
            .expect("tab should start in source window");

        mux.move_tab_between_windows(tab_id, dst_window_id, Some(0))
            .expect("metadata move should succeed");

        assert_eq!(mux.window_containing_tab(tab_id), Some(dst_window_id));
        assert!(
            mux.get_tab(tab_id)
                .map(|stored| Arc::ptr_eq(&stored, &tab))
                .unwrap_or(false),
            "move must keep the same live Arc<Tab> in the mux registry",
        );
        assert!(
            mux.get_pane(401).is_some(),
            "move must keep the tab's pane registered",
        );
        assert_eq!(
            kills.load(Ordering::SeqCst),
            0,
            "metadata move must not kill the pane",
        );

        drop(dst_window);
        drop(src_window);
        Mux::shutdown();
    }

    #[test]
    fn remove_tab_local_only_drops_mirror_without_killing_pane() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let local_only_window = mux.new_empty_window(Some("winunify".to_string()), None);
        let local_only_window_id = *local_only_window;
        let (local_only_tab, local_only_kills) = tab_with_kill_counter(&mux, 501);
        let local_only_tab_id = local_only_tab.tab_id();
        mux.add_tab_to_window(&local_only_tab, local_only_window_id)
            .expect("local-only tab should be attached to a window");

        let normal_window = mux.new_empty_window(Some("winunify".to_string()), None);
        let normal_window_id = *normal_window;
        let (normal_tab, normal_kills) = tab_with_kill_counter(&mux, 502);
        let normal_tab_id = normal_tab.tab_id();
        mux.add_tab_to_window(&normal_tab, normal_window_id)
            .expect("normal tab should be attached to a window");

        let removed = mux
            .remove_tab_local_only(local_only_tab_id)
            .expect("local-only tab should be removed");
        assert!(Arc::ptr_eq(&removed, &local_only_tab));
        assert!(mux.get_tab(local_only_tab_id).is_none());
        assert!(mux.get_pane(501).is_none());
        assert_eq!(
            local_only_kills.load(Ordering::SeqCst),
            0,
            "local-only tab removal must not call Pane::kill / Pdu::KillPane path",
        );

        let normal_removed = mux
            .remove_tab(normal_tab_id)
            .expect("normal tab should be removed");
        assert!(Arc::ptr_eq(&normal_removed, &normal_tab));
        assert_eq!(
            normal_kills.load(Ordering::SeqCst),
            1,
            "ordinary tab removal remains the killing path",
        );

        drop(normal_window);
        drop(local_only_window);
        Mux::shutdown();
    }

    #[test]
    fn detached_domain_is_removed_from_domain_maps() {
        let default_domain: Arc<dyn Domain> =
            Arc::new(domain::LocalDomain::new("default-test-domain").unwrap());
        let mux = Mux::new(Some(Arc::clone(&default_domain)));

        let detached_domain: Arc<dyn Domain> =
            Arc::new(domain::LocalDomain::new("detached-test-domain").unwrap());
        let detached_id = detached_domain.domain_id();
        let detached_name = detached_domain.domain_name().to_string();

        mux.add_domain(&detached_domain)
            .expect("detached test domain should register");
        assert!(mux.get_domain(detached_id).is_some());
        assert!(mux.get_domain_by_name(&detached_name).is_some());

        mux.domain_was_detached(detached_id);

        assert!(mux.get_domain(detached_id).is_none());
        assert!(mux.get_domain_by_name(&detached_name).is_none());
        assert!(mux.get_domain(default_domain.domain_id()).is_some());
    }

    #[test]
    fn detaching_default_domain_promotes_remaining_domain() {
        let default_domain: Arc<dyn Domain> =
            Arc::new(domain::LocalDomain::new("default-domain-to-detach").unwrap());
        let mux = Mux::new(Some(Arc::clone(&default_domain)));

        let replacement_domain: Arc<dyn Domain> =
            Arc::new(domain::LocalDomain::new("replacement-domain").unwrap());
        let replacement_id = replacement_domain.domain_id();
        mux.add_domain(&replacement_domain)
            .expect("replacement test domain should register");

        mux.domain_was_detached(default_domain.domain_id());

        assert!(mux.get_domain(default_domain.domain_id()).is_none());
        assert!(mux.get_domain(replacement_id).is_some());
        assert_eq!(mux.default_domain().domain_id(), replacement_id);
    }

    #[test]
    fn detaching_tmux_domain_eagerly_removes_notification_subscriber() {
        let default_domain: Arc<dyn Domain> =
            Arc::new(domain::LocalDomain::new("default-domain-tmux-detach-test").unwrap());
        let mux = Mux::new(Some(default_domain));

        let tmux_domain = Arc::new(TmuxDomain::new(0));
        let tmux_domain_dyn: Arc<dyn Domain> = tmux_domain.clone();
        let tmux_domain_id = tmux_domain_dyn.domain_id();
        mux.add_domain(&tmux_domain_dyn)
            .expect("tmux test domain should register");

        let sub_id = mux
            .subscribe(|_| true)
            .expect("test mux subscription should allocate an identifier");
        *tmux_domain.inner.notification_sub_id.lock() = Some(sub_id);

        mux.domain_was_detached(tmux_domain_id);

        assert!(mux.get_domain(tmux_domain_id).is_none());
        assert!(
            !mux.unsubscribe(sub_id),
            "tmux notification subscriber should be removed eagerly on detach"
        );
        assert!(tmux_domain.inner.notification_sub_id.lock().is_none());
    }

    #[test]
    fn add_domain_rejects_live_same_name_domain_without_half_detach() {
        let default_domain: Arc<dyn Domain> =
            Arc::new(domain::LocalDomain::new("default-domain-add-domain-test").unwrap());
        let mux = Mux::new(Some(default_domain));

        let first: Arc<dyn Domain> =
            Arc::new(domain::LocalDomain::new("duplicate-name-domain").unwrap());
        let second: Arc<dyn Domain> =
            Arc::new(domain::LocalDomain::new("duplicate-name-domain").unwrap());
        let first_id = first.domain_id();
        let second_id = second.domain_id();

        mux.add_domain(&first)
            .expect("first duplicate-name test domain should register");
        assert!(mux.get_domain(first_id).is_some());
        assert_eq!(
            mux.get_domain_by_name("duplicate-name-domain")
                .unwrap()
                .domain_id(),
            first_id
        );

        let error = mux
            .add_domain(&second)
            .expect_err("a live same-name domain must not be silently half-detached");

        assert_eq!(
            error,
            DomainRegistrationError::NameInUse {
                domain_name: "duplicate-name-domain".to_string(),
                registered_id: first_id,
                requested_id: second_id,
            }
        );
        assert!(mux.get_domain(first_id).is_some());
        assert!(mux.get_domain(second_id).is_none());
        assert_eq!(
            mux.get_domain_by_name("duplicate-name-domain")
                .unwrap()
                .domain_id(),
            first_id
        );
        assert!(
            !mux.retired_domain_ids.lock().contains(&first_id),
            "registration rejection must not retire or strand the live domain",
        );
    }

    #[test]
    fn mux_notification_tab_title_changed() {
        let n = MuxNotification::TabTitleChanged {
            tab_id: 3,
            title: "new title".to_string(),
        };
        let n2 = n.clone();
        let dbg = format!("{:?}", n2);
        assert!(dbg.contains("TabTitleChanged"));
        assert!(dbg.contains("new title"));
    }

    #[test]
    fn mux_notification_window_title_changed() {
        let n = MuxNotification::WindowTitleChanged {
            window_id: 1,
            title: "window title".to_string(),
        };
        let dbg = format!("{:?}", n);
        assert!(dbg.contains("WindowTitleChanged"));
    }

    #[test]
    fn mux_notification_workspace_renamed() {
        let n = MuxNotification::WorkspaceRenamed {
            old_workspace: "old".to_string(),
            new_workspace: "new".to_string(),
        };
        let dbg = format!("{:?}", n);
        assert!(dbg.contains("WorkspaceRenamed"));
        assert!(dbg.contains("old"));
        assert!(dbg.contains("new"));
    }

    #[test]
    fn mux_notification_pane_focused() {
        let n = MuxNotification::PaneFocused(7);
        let dbg = format!("{:?}", n);
        assert!(dbg.contains("PaneFocused"));
        assert!(dbg.contains("7"));
    }

    #[test]
    fn mux_notification_tab_resized() {
        let n = MuxNotification::TabResized(2);
        let dbg = format!("{:?}", n);
        assert!(dbg.contains("TabResized"));
    }

    #[test]
    fn mux_notification_save_to_downloads() {
        let n = MuxNotification::SaveToDownloads {
            name: Some("file.txt".to_string()),
            data: Arc::new(vec![1, 2, 3]),
        };
        let n2 = n.clone();
        let dbg = format!("{:?}", n2);
        assert!(dbg.contains("SaveToDownloads"));
        assert!(dbg.contains("file.txt"));
    }

    #[test]
    fn mux_notification_tab_added_to_window() {
        let n = MuxNotification::TabAddedToWindow {
            tab_id: 1,
            window_id: 2,
        };
        let dbg = format!("{:?}", n);
        assert!(dbg.contains("TabAddedToWindow"));
    }

    #[test]
    fn session_terminated_window_closed_display() {
        let err = SessionTerminated::WindowClosed;
        assert_eq!(format!("{}", err), "Window Closed");
    }

    #[test]
    fn session_terminated_window_closed_debug() {
        let err = SessionTerminated::WindowClosed;
        let dbg = format!("{:?}", err);
        assert!(dbg.contains("WindowClosed"));
    }

    #[test]
    fn session_terminated_is_error() {
        let err = SessionTerminated::WindowClosed;
        let error: &dyn std::error::Error = &err;
        assert_eq!(error.to_string(), "Window Closed");
    }

    #[test]
    fn terminal_size_to_pty_size_basic() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let pty_size = terminal_size_to_pty_size(size).unwrap();
        assert_eq!(pty_size.rows, 24);
        assert_eq!(pty_size.cols, 80);
        assert_eq!(pty_size.pixel_width, 800);
        assert_eq!(pty_size.pixel_height, 600);
    }

    #[test]
    fn terminal_size_to_pty_size_zero() {
        let size = TerminalSize {
            rows: 0,
            cols: 0,
            pixel_width: 0,
            pixel_height: 0,
            dpi: 0,
        };
        let pty_size = terminal_size_to_pty_size(size).unwrap();
        assert_eq!(pty_size.rows, 0);
        assert_eq!(pty_size.cols, 0);
    }

    #[test]
    fn panicking_subscriber_is_removed_and_does_not_poison_others() {
        let mux = Mux::new(None);
        let healthy_count = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&healthy_count);

        // Panicking subscriber
        mux.subscribe(move |_| {
            panic!("intentional test panic in subscriber");
        })
        .expect("test mux subscription should allocate an identifier");

        // Healthy subscriber registered after the panicker
        mux.subscribe(move |_| {
            observed.fetch_add(1, Ordering::Relaxed);
            true
        })
        .expect("test mux subscription should allocate an identifier");

        // First dispatch: panicker fires and is removed, healthy fires
        mux.dispatch_notification(MuxNotification::Empty);
        assert_eq!(healthy_count.load(Ordering::Relaxed), 1);

        // Second dispatch: panicker is gone, only healthy fires
        mux.dispatch_notification(MuxNotification::Empty);
        assert_eq!(healthy_count.load(Ordering::Relaxed), 2);

        // Only the healthy subscriber remains
        assert_eq!(mux.subscribers.read().len(), 1);
    }
}
