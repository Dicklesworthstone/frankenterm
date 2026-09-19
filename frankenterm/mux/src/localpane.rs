use crate::domain::DomainId;
use crate::guardian_checkpoint::{
    LiveParserCaptureAuthority, LiveParserCheckpointAck, LiveParserPaneCaptureError,
};
#[cfg(test)]
use crate::pane::GuardianLiveOutputDelivery;
use crate::pane::{
    CachePolicy, CloseReason, ForEachPaneLogicalLine, GuardianLiveCheckpointPublisher,
    GuardianLiveOutputReader, LogicalLine, Pane, PaneId, PaneSurfaceSnapshot, PaneTitleMetadata,
    Pattern, SearchResult, WithPaneLines,
};
use crate::renderable::*;
use crate::tmux::{TmuxDomain, TmuxDomainState};
use crate::{Domain, PaneRegistrationHandle, PaneRegistrationSlot};
use anyhow::Error;
use async_trait::async_trait;
use config::keyassignment::ScrollbackEraseMode;
use config::{configuration, ExitBehavior, ExitBehaviorMessaging};
use fancy_regex::Regex;
use frankenterm_dynamic::Value;
use frankenterm_sigpipe::{catch_recoverable, RecoverablePanicSite};
use frankenterm_term::color::ColorPalette;
#[cfg(test)]
use frankenterm_term::terminalstate::checkpoint::TerminalCheckpointV2;
use frankenterm_term::terminalstate::checkpoint::{
    TerminalCheckpointError, TerminalCheckpointLimits,
};
use frankenterm_term::{
    Alert, AlertHandler, Clipboard, DownloadHandler, KeyCode, KeyModifiers, MouseEvent, Progress,
    RecoveryTerminalCheckpointError, RecoveryTerminalCheckpointV2, SemanticZone, StableRowIndex,
    Terminal, TerminalConfiguration, TerminalSize,
};
use parking_lot::{MappedMutexGuard, Mutex, MutexGuard};
use portable_pty::{Child, ChildKiller, ExitStatus, MasterPty, PtySize};
use procinfo::LocalProcessInfo;
use rangeset::RangeSet;
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::convert::{TryFrom, TryInto};
use std::io::{Result as IoResult, Write};
use std::ops::Range;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};
use termwiz::escape::csi::{Sgr, CSI};
use termwiz::escape::{Action, DeviceControlMode};
use termwiz::input::KeyboardEncoding;
use termwiz::surface::{Line, SequenceNo};
use url::Url;
use uuid::Uuid;

#[cfg(feature = "disruptor-pane-io")]
use crossbeam::queue::ArrayQueue;

const PROC_INFO_CACHE_TTL: Duration = Duration::from_millis(300);
const LOCAL_PANE_MAIN_THREAD_ESTIMATED_BYTES: usize = 4 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionAnchorCaptureError {
    Busy,
    SourceChanged,
}

type LineLayoutObservation = Mutex<
    Option<(
        frankenterm_term::screen::ScreenCoordinateWitness,
        SequenceNo,
    )>,
>;

/// Owned native paint input. Terminal metadata, damage and resident rows are
/// captured under one nonblocking terminal acquisition. Rendering never holds
/// that lock, and must retain damage if capture returns `None`.
pub struct NativeRenderFrame {
    pub layout_floor: SequenceNo,
    pub source_sequence: SequenceNo,
    pub dimensions: RenderableDimensions,
    pub cursor: StableCursorPosition,
    pub palette: ColorPalette,
    pub dirty: RangeSet<StableRowIndex>,
    pub selection_dirty: RangeSet<StableRowIndex>,
    pub first: StableRowIndex,
    pub lines: Vec<Line>,
    pub password_input: bool,
    coordinate_witness: frankenterm_term::screen::ScreenCoordinateWitness,
}

impl WithPaneLines for NativeRenderFrame {
    fn with_lines_mut(&mut self, first: StableRowIndex, lines: &mut [&mut Line]) {
        self.first = first;
        self.lines.extend(lines.iter().map(|line| (**line).clone()));
    }
}

struct ColdViewportEntry {
    registration: [u8; 16],
    requested: Range<StableRowIndex>,
    read: Arc<frankenterm_term::screen::ScreenLineRead>,
}

type ColdViewportRetired = (
    Arc<frankenterm_term::screen::ScreenLineRead>,
    Vec<ColdViewportEntry>,
);

// Return both rejected publications and evictions to the originating worker.
// That worker keeps its permit until this queue is drained and destroyed.
struct ColdViewportRetirement {
    read: Option<Arc<frankenterm_term::screen::ScreenLineRead>>,
    evicted: Vec<ColdViewportEntry>,
    sender: std::sync::mpsc::SyncSender<ColdViewportRetired>,
}

impl Drop for ColdViewportRetirement {
    fn drop(&mut self) {
        if let Some(read) = self.read.take() {
            let _ = self.sender.send((read, std::mem::take(&mut self.evicted)));
        }
    }
}

// Global, not per pane: a fleet cannot multiply the retained payload allowance.
// Four entries at the shared 32MiB serialized-payload limit. Active workers and
// queued publications have their independent four-permit admission bound.
static COLD_VIEWPORT_CACHE: Mutex<std::collections::VecDeque<ColdViewportEntry>> =
    Mutex::new(std::collections::VecDeque::new());

static COLD_VIEWPORT_RETRIES: AtomicUsize = AtomicUsize::new(0);

struct ColdViewportRetry(Arc<AtomicBool>);

impl Drop for ColdViewportRetry {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
        COLD_VIEWPORT_RETRIES.fetch_sub(1, Ordering::AcqRel);
    }
}

fn retry_cold_viewport(registration: PaneRegistrationHandle, pending: Arc<AtomicBool>) {
    if pending
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    if COLD_VIEWPORT_RETRIES
        .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
            if count < 32 {
                Some(count + 1)
            } else {
                None
            }
        })
        .is_err()
    {
        pending.store(false, Ordering::Release);
        return;
    }
    let retry = ColdViewportRetry(pending);
    schedule_local_pane_main_thread(
        promise::spawn::MainThreadServiceClass::Interactive,
        LOCAL_PANE_MAIN_THREAD_ESTIMATED_BYTES,
        "cold_viewport_retry",
        || async move {
            promise::spawn::sleep(Duration::from_millis(100)).await;
            drop(retry);
            let _ = registration.try_with_current(|pane| pane.notify_lines_ready());
        },
    );
}

struct ColdViewportPending {
    requested: Range<StableRowIndex>,
    cancelled: Arc<AtomicBool>,
}

struct ColdViewportFailure {
    requested: Range<StableRowIndex>,
    witness: frankenterm_term::screen::LineReadFailureWitness,
}

struct ColdViewportCompletion {
    state: Arc<Mutex<Option<ColdViewportPending>>>,
    cancelled: Arc<AtomicBool>,
}

impl Drop for ColdViewportCompletion {
    fn drop(&mut self) {
        let mut state = self.state.lock();
        if state
            .as_ref()
            .is_some_and(|pending| Arc::ptr_eq(&pending.cancelled, &self.cancelled))
        {
            *state = None;
        }
    }
}

fn schedule_local_pane_main_thread<MAKE, FUT>(
    service_class: promise::spawn::MainThreadServiceClass,
    estimated_bytes: usize,
    operation: &'static str,
    make_future: MAKE,
) -> bool
where
    MAKE: FnOnce() -> FUT,
    FUT: std::future::Future<Output = ()> + Send + 'static,
{
    match promise::spawn::try_reserve_main_thread(service_class, estimated_bytes) {
        promise::spawn::MainThreadReservationOutcome::Reserved(reservation) => {
            reservation.spawn(make_future()).detach();
            true
        }
        rejected => {
            metrics::counter!(
                "mux.local_pane.main_thread_admission",
                "operation" => operation,
                "outcome" => "terminal_rejection"
            )
            .increment(1);
            log::error!(
                "main-thread scheduler rejected local-pane {operation} before task construction: {rejected:?}"
            );
            false
        }
    }
}

/// ft-87qfi: capacity (in action batches) of the lock-free SPSC staging ring
/// used on the pane->render hot path when `disruptor-pane-io` is enabled. Each
/// slot holds one parsed `Vec<Action>` batch; when the ring saturates the
/// producer falls back to a blocking apply (back-pressure). Sized to absorb a
/// few render frames of buffered output without unbounded memory growth.
#[cfg(feature = "disruptor-pane-io")]
const PANE_ACTION_RING_CAPACITY: usize = 1024;

#[derive(Debug)]
enum ProcessState {
    Running {
        child_waiter: Receiver<IoResult<ExitStatus>>,
        pid: Option<u32>,
        signaller: Box<dyn ChildKiller + Sync>,
        // Whether we've explicitly killed the child
        killed: bool,
    },
    DeadPendingClose {
        killed: bool,
    },
    Dead,
}

/// Immutable authority identifying one guardian lease held by this mux.
///
/// The mutation sequence is deliberately not exposed here: the concrete
/// guardian proxy owns and serializes that moving fence.  LocalPane retains
/// only the stable identity needed to ensure that a stale generation cannot
/// accidentally target a same-UUID successor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GuardianPaneLeaseIdentity {
    guardian_incarnation: Uuid,
    mux_incarnation: Uuid,
    pane_id: Uuid,
    generation: u64,
}

impl GuardianPaneLeaseIdentity {
    pub fn new(
        guardian_incarnation: Uuid,
        mux_incarnation: Uuid,
        pane_id: Uuid,
        generation: u64,
    ) -> Result<Self, Error> {
        if guardian_incarnation.is_nil() {
            anyhow::bail!("guardian pane lease has a nil guardian incarnation");
        }
        if mux_incarnation.is_nil() {
            anyhow::bail!("guardian pane lease has a nil mux incarnation");
        }
        if pane_id.is_nil() {
            anyhow::bail!("guardian pane lease has a nil durable pane id");
        }
        if generation == 0 {
            anyhow::bail!("guardian pane lease generation must be nonzero");
        }
        Ok(Self {
            guardian_incarnation,
            mux_incarnation,
            pane_id,
            generation,
        })
    }

    #[must_use]
    pub const fn guardian_incarnation(self) -> Uuid {
        self.guardian_incarnation
    }

    #[must_use]
    pub const fn mux_incarnation(self) -> Uuid {
        self.mux_incarnation
    }

    #[must_use]
    pub const fn pane_id(self) -> Uuid {
        self.pane_id
    }

    #[must_use]
    pub const fn generation(self) -> u64 {
        self.generation
    }
}

/// Exact lifetime operations for a guardian-backed LocalPane.
///
/// Implementations must bind `identity` to the same guardian lease actor used
/// by the supplied `MasterPty`, writer, `Child`, and `ChildKiller` proxies.
/// They must serialize the current mutation sequence and use stable
/// request/effect identities so an ambiguous retry is idempotent.  `retire`
/// releases only the mux lease; it must never signal or close the child.
pub trait GuardianPaneLeaseControl: Send + Sync {
    fn close(&self, identity: GuardianPaneLeaseIdentity) -> Result<(), Error>;
    fn retire(&self, identity: GuardianPaneLeaseIdentity) -> Result<(), Error>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GuardianLeaseDisposition {
    Attached,
    ExplicitCloseRequested,
    RetirementRequested,
}

struct GuardianPaneOwnership {
    identity: GuardianPaneLeaseIdentity,
    spawn_custody: Option<crate::guardian_checkpoint::GuardianSpawnCustodyScopeV1>,
    control: Arc<dyn GuardianPaneLeaseControl>,
    disposition: Mutex<GuardianLeaseDisposition>,
}

/// Authority token for model-only legacy pane checkpoint capture.
/// This authority is strictly distinct from guardian capture authority:
/// legacy panes never participate in guardian output journal replication
/// and must never manufacture or use fake guardian output receipts.
#[derive(Clone, Copy, Debug)]
pub struct ModelParserCaptureAuthority {
    _private: (),
}

impl ModelParserCaptureAuthority {
    /// Issue model-only capture authority for legacy mux-owned pane checkpoints.
    pub(crate) fn issue() -> Self {
        Self { _private: () }
    }
}

/// Policy for handling pending parser actions during legacy terminal checkpoint capture.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PendingActionDrainPolicy {
    /// Drain and apply all pending actions to the terminal model before capture.
    DrainAndApply,
    /// Require that all pending actions have already been drained.
    /// If any actions remain, capture fails immediately with `PendingActionsRemain`.
    RequireEmpty,
}

/// Errors occurring during legacy mux-owned terminal checkpoint capture.
#[derive(Debug, thiserror::Error)]
pub enum LegacyTerminalCaptureError {
    #[error("pending parser actions remain unapplied: {0} actions pending")]
    PendingActionsRemain(usize),
    #[error("cold scrollback snapshot generation is stale")]
    StaleColdGeneration,
    #[error("terminal checkpoint error: {0}")]
    Terminal(#[from] RecoveryTerminalCheckpointError),
    #[error(
        "cannot use legacy model capture on guardian-owned pane: false guardian authority rejected"
    )]
    FalseGuardianAuthority,
}

enum LocalPaneOwnership {
    LegacyMuxOwned,
    Guardian(Box<GuardianPaneOwnership>),
}

impl LocalPaneOwnership {
    fn guardian(
        identity: GuardianPaneLeaseIdentity,
        control: Arc<dyn GuardianPaneLeaseControl>,
        spawn_custody: Option<crate::guardian_checkpoint::GuardianSpawnCustodyScopeV1>,
    ) -> Self {
        Self::Guardian(Box::new(GuardianPaneOwnership {
            identity,
            spawn_custody,
            control,
            disposition: Mutex::new(GuardianLeaseDisposition::Attached),
        }))
    }

    /// Return true when guardian ownership handled the explicit close path.
    /// The local transition happens before the fallible transport call so an
    /// indeterminate close can never be followed by lease retirement or a
    /// second, differently identified close from this LocalPane.
    fn request_explicit_close(&self, pane_id: PaneId) -> bool {
        let Self::Guardian(ownership) = self else {
            return false;
        };
        let should_close = {
            let mut disposition = ownership.disposition.lock();
            if *disposition == GuardianLeaseDisposition::Attached {
                *disposition = GuardianLeaseDisposition::ExplicitCloseRequested;
                true
            } else {
                false
            }
        };
        if should_close {
            match catch_recoverable(
                RecoverablePanicSite::MuxPaneCallback,
                AssertUnwindSafe(|| ownership.control.close(ownership.identity)),
            ) {
                Ok(Ok(())) => {}
                Ok(Err(error)) => log::error!(
                    "guardian close failed for local pane {pane_id}, durable pane {}, generation {}: {error:#}",
                    ownership.identity.pane_id(),
                    ownership.identity.generation(),
                ),
                Err(_) => log::error!(
                    "guardian close panicked for local pane {pane_id}, durable pane {}, generation {}; preserving the close fence",
                    ownership.identity.pane_id(),
                    ownership.identity.generation(),
                ),
            }
        }
        true
    }

    /// Return true for every guardian-owned pane, including one whose close is
    /// already pending.  That lets Drop unconditionally skip the legacy child
    /// killer while sending at most one lease-retirement request for a merely
    /// attached pane.
    fn retire_on_drop(&self, pane_id: PaneId) -> bool {
        let Self::Guardian(ownership) = self else {
            return false;
        };
        let should_retire = {
            let mut disposition = ownership.disposition.lock();
            if *disposition == GuardianLeaseDisposition::Attached {
                *disposition = GuardianLeaseDisposition::RetirementRequested;
                true
            } else {
                false
            }
        };
        if should_retire {
            match catch_recoverable(
                RecoverablePanicSite::MuxPaneCallback,
                AssertUnwindSafe(|| ownership.control.retire(ownership.identity)),
            ) {
                Ok(Ok(())) => {}
                Ok(Err(error)) => log::error!(
                    "guardian lease retirement failed for local pane {pane_id}, durable pane {}, generation {}: {error:#}",
                    ownership.identity.pane_id(),
                    ownership.identity.generation(),
                ),
                Err(_) => log::error!(
                    "guardian lease retirement panicked for local pane {pane_id}, durable pane {}, generation {}; child ownership remains with the guardian",
                    ownership.identity.pane_id(),
                    ownership.identity.generation(),
                ),
            }
        }
        true
    }
}

struct CachedProcInfo {
    root: LocalProcessInfo,
    updated: Instant,
    foreground: LocalProcessInfo,
    /// Memoized "is this pane's process tree stateful?" decision.
    /// `None` until the first `can_close_without_prompting` consumer evaluates
    /// it; `Some(b)` afterward — reused for subsequent close attempts within
    /// the cache TTL so the synchronous `mux-is-process-stateful` Lua hook
    /// runs at most once per refresh, not once per close attempt. Reset
    /// implicitly to `None` when the warm worker replaces the whole struct.
    /// See ft-qhwpq.
    cached_is_stateful: Option<bool>,
}

/// Owns one close-time process-cache warm admission.
///
/// The flag must be released on every worker exit, including a stale pane
/// registration, process-tree lookup failure, thread spawn failure, or panic.
/// Keeping that responsibility in `Drop` prevents a failed warm from
/// permanently suppressing all later close-time refreshes.
struct ProcListWarmPendingGuard {
    pending: Arc<AtomicBool>,
}

impl ProcListWarmPendingGuard {
    fn try_acquire(pending: &Arc<AtomicBool>) -> Option<Self> {
        pending
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;
        Some(Self {
            pending: Arc::clone(pending),
        })
    }
}

impl Drop for ProcListWarmPendingGuard {
    fn drop(&mut self) {
        self.pending.store(false, Ordering::Release);
    }
}

#[derive(Default)]
struct ChildExitPruneTracker {
    child_exited: bool,
    current_intent: Option<Arc<()>>,
    completed_intent: Option<Arc<()>>,
    scheduled: bool,
    failed_registration: Option<PaneRegistrationHandle>,
}

impl ChildExitPruneTracker {
    fn record_child_exit(&mut self) {
        self.child_exited = true;
        self.current_intent = Some(Arc::new(()));
        self.failed_registration = None;
    }

    fn record_registration_bound(&mut self) -> bool {
        self.failed_registration = None;
        if !self.child_exited {
            return false;
        }
        self.current_intent = Some(Arc::new(()));
        true
    }

    fn has_pending_intent(&self) -> bool {
        self.current_intent.as_ref().is_some_and(|current| {
            self.completed_intent
                .as_ref()
                .is_none_or(|completed| !Arc::ptr_eq(current, completed))
        })
    }

    fn record_success(&mut self, target_intent: &Arc<()>) {
        self.completed_intent = Some(Arc::clone(target_intent));
        self.failed_registration = None;
    }
}

/// Lossless bridge between the child waiter and mux publication.
///
/// A very short-lived process can exit before its pane registration is
/// published. Loading the slot only from the waiter would then lose the prune
/// nudge forever. This state records the exit independently and lets the
/// post-publication hook schedule it once an exact registration exists.
///
/// Intents carry allocation identities because the same pane object may be
/// registered again after its prior generation retires. A prune accepted for
/// one generation must not consume a concurrent bind intent for its successor;
/// pointer identity avoids finite counter wraparound in very long sessions.
struct ChildExitPruneState {
    mux_registration: Arc<PaneRegistrationSlot>,
    tracker: Mutex<ChildExitPruneTracker>,
}

impl ChildExitPruneState {
    fn new(mux_registration: Arc<PaneRegistrationSlot>) -> Arc<Self> {
        Arc::new(Self {
            mux_registration,
            tracker: Mutex::new(ChildExitPruneTracker::default()),
        })
    }

    fn mark_child_exited(self: &Arc<Self>) {
        self.tracker.lock().record_child_exit();
        self.try_schedule();
    }

    fn registration_bound(self: &Arc<Self>, registration: &PaneRegistrationHandle) {
        let should_schedule = self.tracker.lock().record_registration_bound();
        if should_schedule {
            self.try_schedule_with_registration(Some(registration.clone()));
        }
    }

    fn try_schedule(self: &Arc<Self>) {
        self.try_schedule_with_registration(self.mux_registration.load());
    }

    fn try_schedule_with_registration(
        self: &Arc<Self>,
        registration: Option<PaneRegistrationHandle>,
    ) {
        if !promise::spawn::is_scheduler_configured() {
            return;
        }
        let Some(registration) = registration else {
            return;
        };
        // The waiter may run after the pane (or its mux) was destroyed. Do
        // not let an obsolete exit consume admission in a later scheduler
        // phase. Keep the intent for a legitimate same-pane rebind, and still
        // revalidate at dispatch because retirement can race this check.
        if registration.try_with_current(|_| ()).is_none() {
            return;
        }

        let target_intent = {
            let mut tracker = self.tracker.lock();
            if !tracker.child_exited
                || !tracker.has_pending_intent()
                || tracker.scheduled
                || tracker
                    .failed_registration
                    .as_ref()
                    .is_some_and(|failed| failed.same_registration(&registration))
            {
                return;
            }
            let Some(target_intent) = tracker.current_intent.as_ref().map(Arc::clone) else {
                // `has_pending_intent` above makes this unreachable under the
                // tracker invariant, but scheduling is a background recovery
                // path and must fail closed rather than panic if future edits
                // ever violate that invariant.
                return;
            };
            tracker.scheduled = true;
            target_intent
        };

        let dispatch = ChildExitPruneDispatch {
            state: Arc::clone(self),
            registration: Some(registration),
            target_intent,
            finished: false,
        };
        schedule_local_pane_main_thread(
            promise::spawn::MainThreadServiceClass::Topology,
            LOCAL_PANE_MAIN_THREAD_ESTIMATED_BYTES,
            "child-exit prune",
            move || async move {
                dispatch.execute();
            },
        );
    }

    fn finish_dispatch(
        self: &Arc<Self>,
        target_intent: &Arc<()>,
        registration: &PaneRegistrationHandle,
        pruned: bool,
    ) {
        let needs_retry = {
            let mut tracker = self.tracker.lock();
            tracker.scheduled = false;
            if pruned {
                tracker.record_success(target_intent);
            } else {
                tracker.failed_registration = Some(registration.clone());
            }
            tracker.has_pending_intent()
        };
        if !needs_retry {
            return;
        }

        let current = self.mux_registration.load();
        let registration_changed = current
            .as_ref()
            .is_some_and(|current| !current.same_registration(registration));
        if pruned || registration_changed {
            self.try_schedule_with_registration(current);
        }
    }

    fn abandon_dispatch(&self) {
        self.tracker.lock().scheduled = false;
    }
}

/// Makes scheduler rejection/cancellation release the single-flight slot.
///
/// The exit intent remains pending and can be retried by the bind hook or a
/// later `is_dead` probe. We intentionally do not prune inline from `Drop`,
/// because the rejected future can be dropped on a non-main child-waiter
/// thread.
struct ChildExitPruneDispatch {
    state: Arc<ChildExitPruneState>,
    registration: Option<PaneRegistrationHandle>,
    target_intent: Arc<()>,
    finished: bool,
}

impl ChildExitPruneDispatch {
    fn execute(mut self) {
        let registration = self
            .registration
            .take()
            .expect("child-exit prune dispatch executes at most once");
        let pruned = registration
            .try_with_current(|pane| {
                pane.prune_dead_windows();
            })
            .is_some();
        self.state
            .finish_dispatch(&self.target_intent, &registration, pruned);
        self.finished = true;
    }
}

impl Drop for ChildExitPruneDispatch {
    fn drop(&mut self) {
        if !self.finished {
            self.state.abandon_dispatch();
        }
    }
}

/// Walks a process tree to find the most-recently-started descendant.
///
/// On Windows, children with `console == 0` are skipped so the result reflects
/// the effective foreground process the user is interacting with (Windows has
/// no job control / session leader concept; we approximate it by the youngest
/// console-attached descendant).
///
/// Extracted from `LocalPane::divine_process_list` so the off-main-thread
/// `LocalPane::warm_proc_cache` builds the same `foreground` value the
/// fetch-immediate path would have built — earlier I had a bug where the warm
/// worker fell back to `root.clone()` and broke the Windows
/// `divine_current_working_dir(&fg.cwd)` path. See ft-qhwpq.
fn find_youngest_descendant(root: &LocalProcessInfo) -> &LocalProcessInfo {
    fn recurse<'a>(proc: &'a LocalProcessInfo, youngest: &mut &'a LocalProcessInfo) {
        if proc.start_time >= youngest.start_time {
            *youngest = proc;
        }
        for child in proc.children.values() {
            #[cfg(windows)]
            if child.console == 0 {
                continue;
            }
            recurse(child, youngest);
        }
    }
    let mut youngest = root;
    recurse(root, &mut youngest);
    youngest
}

/// This is a bit horrible; it can take 700us to tcgetpgrp, so if we have
/// 10 tabs open and run the mouse over them, hovering them each in turn,
/// we can spend 7ms per evaluation of the tab bar state on fetching those
/// pids alone, which can easily lead to stuttering when moving the mouse
/// over all of the tabs.
///
/// This implements a cache holding that fg process and the often queried
/// cwd and process path that allows for stale reads to proceed quickly
/// while the writes can happen in a background thread.
#[cfg(unix)]
#[derive(Clone)]
struct CachedLeaderInfo {
    updated: Instant,
    fd: std::os::fd::RawFd,
    pid: u32,
    path: Option<std::path::PathBuf>,
    current_working_dir: Option<std::path::PathBuf>,
    updating: bool,
}

#[cfg(unix)]
impl CachedLeaderInfo {
    fn new(fd: Option<std::os::fd::RawFd>) -> Self {
        let mut me = Self {
            updated: Instant::now(),
            fd: fd.unwrap_or(-1),
            pid: 0,
            path: None,
            current_working_dir: None,
            updating: false,
        };
        me.update();
        me
    }

    fn can_update(&self) -> bool {
        self.fd != -1 && !self.updating
    }

    fn update(&mut self) {
        let raw_pid = unsafe { libc::tcgetpgrp(self.fd) };
        self.pid = if raw_pid > 0 { raw_pid as u32 } else { 0 };
        if self.pid > 0 {
            self.path = LocalProcessInfo::executable_path(self.pid);
            self.current_working_dir = LocalProcessInfo::current_working_dir(self.pid);
        } else {
            self.path.take();
            self.current_working_dir.take();
        }
        self.updated = Instant::now();
        self.updating = false;
    }

    fn expired(&self) -> bool {
        self.updated.elapsed() > PROC_INFO_CACHE_TTL
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum LocalPaneConnectionState {
    Connecting,
    Connected,
}

#[derive(Clone, Copy)]
struct PendingResize {
    seq: u64,
    size: TerminalSize,
    pty_size: PtySize,
    enqueued_at: Instant,
    recoverable_panic_retries: u8,
    apply_error_retries: u8,
}

const MAX_RESIZE_RECOVERABLE_PANIC_RETRIES: u8 = 2;
const MAX_RESIZE_APPLY_ERROR_RETRIES: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ResizeEnqueueOutcome {
    seq: u64,
    replaced_seq: Option<u64>,
    spawn_worker: bool,
    queue_depth_hint: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResizeEnqueueError {
    SequenceExhausted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ResizeCancellationToken {
    seq: u64,
}

impl ResizeCancellationToken {
    fn new(seq: u64) -> Self {
        Self { seq }
    }
}

#[derive(Default)]
struct ResizeQueueState {
    pending: Option<PendingResize>,
    next_seq: u64,
    worker_running: bool,
    /// Only the newest remote pane-size intent may infer its containing tab.
    /// GUI-owned tab resizes must not be overwritten by partially resized
    /// siblings while their asynchronous workers are still completing.
    reconcile_tab_on_completion: bool,
    /// Capacity belongs to `next_seq`, including its bounded worker retries.
    /// Reserve it before accepting a remote intent, not after changing geometry.
    completion_reservation: Option<promise::spawn::MainThreadSpawnReservation>,
    /// Last PTY geometry whose `MasterPty::resize` call completed successfully.
    ///
    /// Terminal geometry alone is not sufficient no-op authority: an older
    /// in-flight intent can resize the PTY and then be superseded before its
    /// terminal commit. The winning intent must reconcile both sides even when
    /// the terminal has already returned to its requested geometry.
    last_proven_pty_size: Option<PtySize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResizeFailureKind {
    RecoverablePanic,
    ApplyError,
}

impl ResizeFailureKind {
    fn retry_limit(self) -> u8 {
        match self {
            Self::RecoverablePanic => MAX_RESIZE_RECOVERABLE_PANIC_RETRIES,
            Self::ApplyError => MAX_RESIZE_APPLY_ERROR_RETRIES,
        }
    }

    fn metric_label(self) -> &'static str {
        match self {
            Self::RecoverablePanic => "recoverable_panic",
            Self::ApplyError => "apply_error",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResizeFailureRecovery {
    Requeued { retry: u8 },
    Superseded { by_seq: u64 },
    ExhaustedRetained { retries: u8 },
}

impl ResizeFailureRecovery {
    fn metric_label(self) -> &'static str {
        match self {
            Self::Requeued { .. } => "requeued",
            Self::Superseded { .. } => "superseded",
            Self::ExhaustedRetained { .. } => "exhausted_retained",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ResizeCommitDecision<T> {
    Committed(T),
    Superseded { by_seq: u64 },
}

fn resize_is_proven_noop(
    current_size: TerminalSize,
    target_size: TerminalSize,
    last_proven_pty_size: Option<PtySize>,
    target_pty_size: PtySize,
) -> bool {
    current_size == target_size && last_proven_pty_size == Some(target_pty_size)
}

impl ResizeQueueState {
    fn try_enqueue(
        &mut self,
        size: TerminalSize,
        pty_size: PtySize,
        enqueued_at: Instant,
        reconcile_tab_on_completion: bool,
        completion_reservation: Option<promise::spawn::MainThreadSpawnReservation>,
    ) -> Result<ResizeEnqueueOutcome, ResizeEnqueueError> {
        let seq = self
            .next_seq
            .checked_add(1)
            .ok_or(ResizeEnqueueError::SequenceExhausted)?;
        let replaced_seq = self.pending.as_ref().map(|pending| pending.seq);
        let spawn_worker = !self.worker_running;
        let queue_depth_hint = if self.worker_running { 2 } else { 1 };

        self.next_seq = seq;
        self.reconcile_tab_on_completion = reconcile_tab_on_completion;
        self.completion_reservation = completion_reservation;
        if spawn_worker {
            self.worker_running = true;
        }

        self.pending = Some(PendingResize {
            seq,
            size,
            pty_size,
            enqueued_at,
            recoverable_panic_retries: 0,
            apply_error_retries: 0,
        });

        Ok(ResizeEnqueueOutcome {
            seq,
            replaced_seq,
            spawn_worker,
            queue_depth_hint,
        })
    }

    #[cfg(test)]
    fn enqueue(
        &mut self,
        size: TerminalSize,
        pty_size: PtySize,
        enqueued_at: Instant,
    ) -> ResizeEnqueueOutcome {
        self.try_enqueue(size, pty_size, enqueued_at, false, None)
            .expect("test resize generation must remain below u64::MAX")
    }

    fn dequeue_for_worker(&mut self) -> Option<PendingResize> {
        if let Some(pending) = self.pending.take() {
            return Some(pending);
        }

        self.worker_running = false;
        self.completion_reservation = None;
        None
    }

    fn superseded_by(&self, token: ResizeCancellationToken) -> Option<u64> {
        // Exactly one intent may be in flight and the queue retains at most
        // one newer coalesced intent. Generation inequality therefore means
        // superseded. `try_enqueue` rejects exhaustion rather than wrapping,
        // so an ancient in-flight token can never alias a current generation.
        (self.next_seq != token.seq).then_some(self.next_seq)
    }

    /// Preserve a dequeued intent after a callback panic or apply error.
    ///
    /// A newer pending intent always wins. Otherwise the exact dequeued
    /// target is retried a bounded number of times. After the budget is
    /// exhausted, retain that target while releasing worker admission: a
    /// future resize can replace it and start a fresh worker, while the last
    /// requested geometry is never silently forgotten behind a stale
    /// `worker_running=true` latch.
    fn recover_failed_intent(
        &mut self,
        mut intent: PendingResize,
        failure: ResizeFailureKind,
    ) -> ResizeFailureRecovery {
        if let Some(newer) = self.pending.as_ref() {
            return ResizeFailureRecovery::Superseded { by_seq: newer.seq };
        }

        let retries = match failure {
            ResizeFailureKind::RecoverablePanic => &mut intent.recoverable_panic_retries,
            ResizeFailureKind::ApplyError => &mut intent.apply_error_retries,
        };
        if *retries < failure.retry_limit() {
            *retries = (*retries).saturating_add(1);
            let retry = *retries;
            self.pending = Some(intent);
            self.worker_running = true;
            ResizeFailureRecovery::Requeued { retry }
        } else {
            let retries = *retries;
            self.pending = Some(intent);
            self.worker_running = false;
            self.completion_reservation = None;
            ResizeFailureRecovery::ExhaustedRetained { retries }
        }
    }
}

fn settle_resize_worker_spawn<T, E>(spawn_result: Result<T, E>, run_inline: impl FnOnce()) {
    if spawn_result.is_err() {
        run_inline();
    }
}

fn catch_resize_intent<T>(
    resize_queue: &Mutex<ResizeQueueState>,
    pending: PendingResize,
    apply: impl FnOnce() -> T,
) -> Result<T, ResizeFailureRecovery> {
    match catch_recoverable(
        RecoverablePanicSite::MuxPaneCallback,
        AssertUnwindSafe(apply),
    ) {
        Ok(result) => Ok(result),
        Err(_) => Err(resize_queue
            .lock()
            .recover_failed_intent(pending, ResizeFailureKind::RecoverablePanic)),
    }
}

fn recover_resize_apply_error<T, E>(
    resize_queue: &Mutex<ResizeQueueState>,
    pending: PendingResize,
    result: Result<T, E>,
) -> Result<T, (E, ResizeFailureRecovery)> {
    result.map_err(|error| {
        let recovery = resize_queue
            .lock()
            .recover_failed_intent(pending, ResizeFailureKind::ApplyError);
        (error, recovery)
    })
}

fn record_resize_failure(kind: ResizeFailureKind, recovery: ResizeFailureRecovery) {
    metrics::counter!(
        "mux.localpane.resize.intent_failure",
        "kind" => kind.metric_label(),
        "settlement" => recovery.metric_label(),
    )
    .increment(1);
}

/// Linearize the last supersession check with its terminal commit.
///
/// The caller acquires the terminal lock before entering this helper. The
/// resulting order is therefore `terminal -> resize_queue`. Enqueue and
/// dequeue paths hold `resize_queue` only long enough to mutate queue state
/// and release it before touching the terminal or spawning a worker; no
/// inverse `resize_queue -> terminal` critical section is permitted. Holding
/// the queue guard through `commit` means a newer intent linearizes either
/// before the check (and rejects this commit) or after the commit, never in
/// the stale check/commit gap.
fn with_resize_commit_barrier<T>(
    resize_queue: &Mutex<ResizeQueueState>,
    token: ResizeCancellationToken,
    commit: impl FnOnce() -> T,
) -> (ResizeCommitDecision<T>, Duration) {
    let wait_start = Instant::now();
    let queue = resize_queue.lock();
    let wait = wait_start.elapsed();
    if let Some(by_seq) = queue.superseded_by(token) {
        return (ResizeCommitDecision::Superseded { by_seq }, wait);
    }
    let value = commit();
    drop(queue);
    (ResizeCommitDecision::Committed(value), wait)
}

/// Used only by the existing resize worker, never the inline spawn-failure
/// fallback. A busy lock/admission is not completion of the newest intent.
/// `None` from the step means explicitly retryable contention or a stale
/// snapshot that must be recaptured; all other failures retain their error.
/// No guard from `step` survives the bounded backoff, and supersession or
/// registration retirement cancels even an indefinitely contended resource.
fn retry_cold_resize_step<T>(
    phase: &'static str,
    cancelled: &impl Fn() -> bool,
    mut step: impl FnMut() -> anyhow::Result<Option<T>>,
) -> anyhow::Result<Option<T>> {
    let mut backoff = Duration::from_millis(1);
    loop {
        if cancelled() {
            return Ok(None);
        }
        let reason = match step() {
            Ok(Some(value)) => return Ok(Some(value)),
            Ok(None) => "contention_or_recapture",
            Err(error) if error.is::<frankenterm_term::screen::ColdReadMetadataBusy>() => {
                "metadata_busy"
            }
            Err(error) => {
                let reason = if error.is::<frankenterm_term::screen::ColdReadGeometryUnavailable>()
                {
                    "geometry_unavailable"
                } else if error.is::<frankenterm_term::screen::ColdReadPayloadLimit>() {
                    "payload_limit"
                } else {
                    "source_unavailable"
                };
                log::debug!("cold resize step failed phase={phase} reason={reason}");
                return Err(error);
            }
        };
        metrics::counter!("mux.localpane.resize.cold_retry", "phase" => phase, "reason" => reason)
            .increment(1);
        if cancelled() {
            return Ok(None);
        }
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(Duration::from_millis(8));
    }
}

fn next_cold_resize_sequence(current: SequenceNo) -> anyhow::Result<SequenceNo> {
    current
        .checked_add(1)
        .filter(|seqno| *seqno != SequenceNo::MAX)
        .ok_or_else(|| anyhow::anyhow!("cold seam sequence exhausted"))
}

#[derive(Clone, Copy)]
struct ResizeApplyMetrics {
    commit_id: u64,
    current_size: TerminalSize,
    target_size: TerminalSize,
    probe_lock_wait: Duration,
    pty_lock_wait: Duration,
    pty_resize_elapsed: Duration,
    pty_resize_attempts: usize,
    pty_retry_backoff_elapsed: Duration,
    swap_barrier_wait: Duration,
    terminal_apply_lock_wait: Duration,
    terminal_resize_elapsed: Duration,
    noop: bool,
    rejected_frame: bool,
    cancelled: bool,
    cancelled_stage: Option<&'static str>,
    superseded_by_seq: Option<u64>,
}

#[derive(Clone, Copy)]
struct ResizeRetryPolicy {
    max_attempts: usize,
    base_backoff: Duration,
    max_backoff: Duration,
}

#[derive(Debug, Clone, Copy, Default)]
struct ResizeRetryStats {
    attempts: usize,
    backoff_elapsed: Duration,
}

enum PtyResizeAttemptFailure {
    Superseded { by_seq: u64 },
    Apply(Error),
}

fn pty_resize_retry_policy() -> ResizeRetryPolicy {
    ResizeRetryPolicy {
        max_attempts: 3,
        base_backoff: Duration::from_millis(2),
        max_backoff: Duration::from_millis(25),
    }
}

fn retry_backoff_for_attempt(policy: ResizeRetryPolicy, attempt: usize) -> Duration {
    if attempt == 0 {
        return Duration::default();
    }

    let shift = attempt.saturating_sub(1).min(20) as u32;
    let factor = 1u32 << shift;
    policy
        .base_backoff
        .saturating_mul(factor)
        .min(policy.max_backoff)
}

fn next_search_grapheme_idx(last_grapheme_idx: usize) -> usize {
    last_grapheme_idx.saturating_add(1)
}

fn next_resize_retry_attempt(attempt: usize) -> usize {
    attempt.saturating_add(1)
}

enum RetryStepError<E> {
    Retry(E),
    Stop(E),
}

fn retry_with_backoff_controlled<T, E, F>(
    policy: ResizeRetryPolicy,
    mut op: F,
) -> Result<(T, ResizeRetryStats), (E, ResizeRetryStats)>
where
    F: FnMut(usize) -> Result<T, RetryStepError<E>>,
{
    let mut stats = ResizeRetryStats::default();
    let max_attempts = policy.max_attempts.max(1);
    let mut attempt = 1;
    loop {
        stats.attempts = attempt;
        match op(attempt) {
            Ok(value) => return Ok((value, stats)),
            Err(RetryStepError::Stop(err)) => return Err((err, stats)),
            Err(RetryStepError::Retry(err)) => {
                if attempt == max_attempts {
                    return Err((err, stats));
                }
                let backoff = retry_backoff_for_attempt(policy, attempt);
                stats.backoff_elapsed = stats.backoff_elapsed.saturating_add(backoff);
                std::thread::sleep(backoff);
                attempt = next_resize_retry_attempt(attempt);
            }
        }
    }
}

#[cfg(test)]
fn retry_with_backoff<T, E, F>(
    policy: ResizeRetryPolicy,
    mut op: F,
) -> Result<(T, ResizeRetryStats), (E, ResizeRetryStats)>
where
    F: FnMut(usize) -> Result<T, E>,
{
    retry_with_backoff_controlled(policy, |attempt| op(attempt).map_err(RetryStepError::Retry))
}

pub struct LocalPane {
    pane_id: PaneId,
    durable_pane_id: [u8; 16],
    ownership: LocalPaneOwnership,
    terminal: Arc<Mutex<Terminal>>,
    // Pane-owned lifetime prevents cached metadata crossing pane-id reuse.
    // Only Arc swaps/clones run under this mutex, never terminal work.
    title_metadata: Mutex<Arc<PaneTitleMetadata>>,
    cold_viewport_pending: Arc<Mutex<Option<ColdViewportPending>>>,
    cold_viewport_retry: Arc<AtomicBool>,
    cold_viewport_failure: Arc<Mutex<Option<ColdViewportFailure>>>,
    line_layout_observation: Arc<LineLayoutObservation>,
    // Serializes complete producer batches, including deferred persistence,
    // without preventing GUI readers or resize workers from taking terminal.
    output_application: Mutex<()>,
    scrollback_flush_sink: Mutex<Option<Arc<dyn frankenterm_term::config::ScrollbackSpillSink>>>,
    process: Arc<Mutex<ProcessState>>,
    pty: Arc<Mutex<Box<dyn MasterPty>>>,
    guardian_live_output_reader: Mutex<Option<Box<dyn GuardianLiveOutputReader>>>,
    guardian_restored_prefix:
        Mutex<Option<crate::guardian_checkpoint::GuardianRestoredParserPrefix>>,
    guardian_checkpoint_publisher: Option<Arc<dyn GuardianLiveCheckpointPublisher>>,
    resize_queue: Arc<Mutex<ResizeQueueState>>,
    writer: Mutex<Box<dyn Write + Send>>,
    domain_id: DomainId,
    tmux_domain: Arc<Mutex<Option<Arc<TmuxDomainState>>>>,
    mux_registration: Arc<PaneRegistrationSlot>,
    child_exit_prune: Arc<ChildExitPruneState>,
    proc_list: Arc<Mutex<Option<CachedProcInfo>>>,
    proc_list_prime_started: AtomicBool,
    /// Single-flight guard for the background warm task that
    /// `can_close_without_prompting` spawns when its cache-only fast path
    /// misses. Prevents stacking N warm tasks if the user closes N tabs in a
    /// burst — one warm runs, the rest just see the in-progress flag and
    /// rely on it populating proc_list. See ft-qhwpq.
    proc_list_warm_pending: Arc<AtomicBool>,
    #[cfg(unix)]
    leader: Arc<Mutex<Option<CachedLeaderInfo>>>,
    command_description: String,
    /// ft-87qfi: lock-free LMAX-disruptor-style SPSC staging ring for parsed
    /// action batches on the pane->render hot path. The SINGLE parser thread is
    /// the producer (via `perform_actions`); the consumer is whichever thread
    /// next locks the terminal (serialized by the terminal mutex, drained FIFO
    /// in `locked_terminal`). Lets the parser stage a batch and keep parsing
    /// instead of blocking on the terminal lock while the renderer reads. Uses
    /// `crossbeam::queue::ArrayQueue` (a safe, vetted lock-free bounded ring —
    /// NO `unsafe`). Present only under the `disruptor-pane-io` feature; the
    /// default build keeps the plain mutex path.
    #[cfg(feature = "disruptor-pane-io")]
    action_ring: Arc<ArrayQueue<Vec<Action>>>,
}

fn record_input_for_current_identity(registration: &PaneRegistrationSlot) {
    if let Some(registration) = registration.load() {
        let _ = registration.try_with_current(|pane| {
            pane.record_input_for_current_identity();
        });
    }
}

#[async_trait(?Send)]
impl Pane for LocalPane {
    fn guardian_spawn_custody(
        &self,
    ) -> Option<crate::guardian_checkpoint::GuardianSpawnCaptureProvenanceV1> {
        match &self.ownership {
            LocalPaneOwnership::Guardian(owner) => owner.spawn_custody.map(|original| {
                crate::guardian_checkpoint::GuardianSpawnCaptureProvenanceV1 {
                    original,
                    current_mux_incarnation: owner.identity.mux_incarnation(),
                    current_lease_generation: owner.identity.generation(),
                }
            }),
            _ => None,
        }
    }
    fn pane_id(&self) -> PaneId {
        self.pane_id
    }

    fn durable_pane_id(&self) -> Option<[u8; 16]> {
        Some(self.durable_pane_id)
    }

    fn get_metadata(&self) -> Value {
        #[allow(unused_mut)]
        let mut map: BTreeMap<Value, Value> = BTreeMap::new();

        #[cfg(unix)]
        if let Some(tio) = self.pty.lock().get_termios() {
            use nix::sys::termios::LocalFlags;
            // Detect whether we might be in password input mode.
            // If local echo is disabled and canonical input mode
            // is enabled, then we assume that we're in some kind
            // of password-entry mode.
            let pw_input = !tio.local_flags.contains(LocalFlags::ECHO)
                && tio.local_flags.contains(LocalFlags::ICANON);
            map.insert(
                Value::String("password_input".to_string()),
                Value::Bool(pw_input),
            );
        }

        Value::Object(map.into())
    }

    fn get_cursor_position(&self) -> StableCursorPosition {
        let mut cursor = terminal_get_cursor_position(&mut self.locked_terminal());
        if self.tmux_domain.lock().is_some() {
            cursor.visibility = termwiz::surface::CursorVisibility::Hidden;
        }
        cursor
    }

    fn get_keyboard_encoding(&self) -> KeyboardEncoding {
        if self.tmux_domain.lock().is_some() {
            KeyboardEncoding::Xterm
        } else {
            self.locked_terminal().get_keyboard_encoding()
        }
    }

    fn get_current_seqno(&self) -> SequenceNo {
        self.locked_terminal().current_seqno()
    }

    fn get_changed_since(
        &self,
        lines: Range<StableRowIndex>,
        seqno: SequenceNo,
    ) -> RangeSet<StableRowIndex> {
        terminal_get_dirty_lines(&mut self.locked_terminal(), lines, seqno)
    }

    fn get_changed_since_with_source_fence(
        &self,
        lines: Range<StableRowIndex>,
        last_observed_source_end: SequenceNo,
    ) -> (SequenceNo, RangeSet<StableRowIndex>) {
        let mut terminal = self.locked_terminal();
        let source_end = terminal.current_seqno();
        let baseline =
            crate::pane::changed_since_query_baseline(last_observed_source_end, source_end);
        let changed = terminal_get_dirty_lines(&mut terminal, lines, baseline);
        (source_end, changed)
    }

    fn for_each_logical_line_in_stable_range_mut(
        &self,
        lines: Range<StableRowIndex>,
        for_line: &mut dyn ForEachPaneLogicalLine,
    ) {
        let mut term = self.locked_terminal();
        let cold = lines.start < term.screen().phys_to_stable_row_index(0);
        if cold {
            drop(term);
            crate::pane::impl_for_each_logical_line_via_get_logical_lines(self, lines, for_line);
            return;
        }
        terminal_for_each_logical_line_in_stable_range_mut(&mut term, lines, for_line);
    }

    fn with_lines_mut(&self, lines: Range<StableRowIndex>, with_lines: &mut dyn WithPaneLines) {
        let mut term = self.locked_terminal();
        let cold = lines.start < term.screen().phys_to_stable_row_index(0);
        if cold {
            drop(term);
            crate::pane::impl_with_lines_via_get_lines(self, lines, with_lines);
            return;
        }
        terminal_with_lines_mut(&mut term, lines, with_lines)
    }

    fn with_lines_mut_and_apply_hyperlinks(
        &self,
        lines: Range<StableRowIndex>,
        rules: &[termwiz::hyperlink::Rule],
        with_lines: &mut dyn WithPaneLines,
    ) {
        // Never substitute resident row zero for a requested persisted row.
        // A missing cold snapshot is a loading frame, followed by a targeted
        // repaint after exact-registration publication of the worker result.
        let Some(mut term) = self.terminal.try_lock() else {
            if let Some(registration) = self.mux_registration.load() {
                retry_cold_viewport(registration, Arc::clone(&self.cold_viewport_retry));
            }
            with_lines.with_lines_mut(lines.start, &mut []);
            return;
        };
        #[cfg(feature = "disruptor-pane-io")]
        self.drain_action_ring_locked(&mut term);
        let cold = lines.start < term.screen().phys_to_stable_row_index(0);
        if cold {
            let logical_context = term.screen().expand_cold_logical_range(lines.clone());
            drop(term);
            let (first, mut snapshot) = self.cold_viewport_lines(logical_context);
            let mut start = 0;
            for end in 0..snapshot.len() {
                if !snapshot[end].last_cell_was_wrapped() || end + 1 == snapshot.len() {
                    Line::apply_hyperlink_rules(
                        rules,
                        &mut snapshot[start..=end].iter_mut().collect::<Vec<_>>(),
                    );
                    start = end + 1;
                }
            }
            let visible_first = lines.start.max(first);
            let skip = visible_first.saturating_sub(first) as usize;
            let count = lines.end.saturating_sub(visible_first).max(0) as usize;
            with_lines.with_lines_mut(
                visible_first,
                &mut snapshot
                    .iter_mut()
                    .skip(skip)
                    .take(count)
                    .collect::<Vec<_>>(),
            );
            return;
        }
        struct Snapshot {
            first: StableRowIndex,
            lines: Vec<Line>,
        }
        impl WithPaneLines for Snapshot {
            fn with_lines_mut(&mut self, first: StableRowIndex, lines: &mut [&mut Line]) {
                self.first = first;
                self.lines.extend(lines.iter().map(|line| (**line).clone()));
            }
        }

        let mut snapshot = Snapshot {
            first: lines.start,
            lines: Vec::new(),
        };
        // Keep the successful nonblocking acquisition through classification
        // and capture. Dropping it and calling locked_terminal here lets a
        // resize win the gap and turn this paint path into a blocking wait.
        terminal_with_lines_mut_and_apply_hyperlinks(&mut term, lines, rules, &mut snapshot);
        let coordinate_witness = term.screen().capture_coordinate_witness();
        drop(term);
        if let Some(pending) = self
            .cold_viewport_pending
            .try_lock()
            .and_then(|mut pending| pending.take())
        {
            pending.cancelled.store(true, Ordering::Release);
        }
        // Shaping, glyph uploads and overlay callbacks must not exclude parser
        // progress or re-enter pane APIs under the terminal mutex. Hyperlinks
        // were applied to the authoritative logical lines before cloning.
        let mut refs = snapshot.lines.iter_mut().collect::<Vec<_>>();
        with_lines.with_lines_mut(snapshot.first, &mut refs);
        drop(refs);

        // Persist only renderer metadata, and only for exactly unchanged rows.
        // A callback may decorate its copy or the parser may have changed the
        // source while rendering. Neither can overwrite terminal content.
        let Some(end) = StableRowIndex::try_from(snapshot.lines.len())
            .ok()
            .and_then(|len| snapshot.first.checked_add(len))
        else {
            return;
        };
        // This is optional cache maintenance, not a terminal read. Never wait
        // for the parser or drain staged actions after the frame is rendered.
        let Some(mut term) = self.terminal.try_lock() else {
            return;
        };
        let screen = term.screen_mut();
        if !screen.matches_coordinate_witness(&coordinate_witness) {
            return;
        }
        let physical = screen.stable_range(&(snapshot.first..end));
        if screen.phys_to_stable_row_index(physical.start) != snapshot.first {
            return;
        }
        screen.with_phys_lines(physical, |current| {
            for (current, rendered) in current.iter().zip(&snapshot.lines) {
                if *current == rendered {
                    current.copy_appdata_from(rendered);
                }
            }
        });
    }

    fn get_lines(&self, lines: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
        // This synchronous API is also used by copy/semantic consumers which
        // treat an empty result as complete. Only paint uses the loading cache;
        // RPCs use owned worker plans. Preserve complete synchronous results
        // until these remaining consumers acquire an awaited read contract.
        terminal_get_lines(&mut self.locked_terminal(), lines)
    }

    fn capture_line_read(
        &self,
        lines: Range<StableRowIndex>,
        budget: &mut frankenterm_term::screen::LineReadCaptureBudget,
    ) -> Option<anyhow::Result<frankenterm_term::screen::ScreenLineRead>> {
        Some(
            self.terminal
                .try_lock()
                .ok_or_else(|| anyhow::Error::new(frankenterm_term::screen::ColdReadMetadataBusy))
                .and_then(|term| term.screen().capture_line_read_with_budget(lines, budget)),
        )
    }

    fn publish_line_reads(
        &self,
        reads: &[frankenterm_term::screen::ScreenLineRead],
        publish: &mut dyn FnMut(),
    ) -> Result<bool, frankenterm_term::screen::ColdReadMetadataBusy> {
        let mut term = self
            .terminal
            .try_lock()
            .ok_or(frankenterm_term::screen::ColdReadMetadataBusy)?;
        for read in reads {
            if !term.screen().try_validate_line_read(read)? {
                return Ok(false);
            }
        }
        let changed = reads
            .iter()
            .any(|read| term.screen().line_read_changes_layout(read));
        if changed {
            if term.current_seqno() == SequenceNo::MAX {
                return Ok(false);
            }
            term.increment_seqno();
        }
        let seqno = term.current_seqno();
        for read in reads {
            term.screen_mut().install_line_read_layout(read, seqno);
        }
        publish();
        Ok(true)
    }

    fn get_line_layout(
        &self,
    ) -> Result<
        Option<(SequenceNo, RenderableDimensions)>,
        frankenterm_term::screen::ColdReadMetadataBusy,
    > {
        let mut term = self
            .terminal
            .try_lock()
            .ok_or(frankenterm_term::screen::ColdReadMetadataBusy)?;
        let Some(floor) =
            Self::refresh_line_layout_floor(&self.line_layout_observation, &mut term)?
        else {
            return Ok(None);
        };
        let Some(dimensions) = terminal_try_get_dimensions(&mut term) else {
            return Ok(None);
        };
        Ok(Some((floor, dimensions)))
    }

    fn publish_line_reads_at_layout(
        &self,
        reads: &[frankenterm_term::screen::ScreenLineRead],
        expected_seqno: SequenceNo,
        expected_dimensions: RenderableDimensions,
        publish: &mut dyn FnMut(),
    ) -> Result<bool, frankenterm_term::screen::ColdReadMetadataBusy> {
        let mut term = self
            .terminal
            .try_lock()
            .ok_or(frankenterm_term::screen::ColdReadMetadataBusy)?;
        let Some(floor) =
            Self::refresh_line_layout_floor(&self.line_layout_observation, &mut term)?
        else {
            return Ok(false);
        };
        let Some(dimensions) = terminal_try_get_dimensions(&mut term) else {
            return Ok(false);
        };
        if expected_seqno == SequenceNo::MAX
            || expected_seqno < floor
            || expected_seqno > term.current_seqno()
            || !crate::renderable::same_line_layout_geometry(&dimensions, &expected_dimensions)
        {
            return Ok(false);
        }
        for read in reads {
            if !term.screen().try_validate_line_read(read)? {
                return Ok(false);
            }
        }
        if reads
            .iter()
            .any(|read| term.screen().line_read_changes_layout(read))
        {
            term.increment_seqno();
            let seqno = term.current_seqno();
            for read in reads {
                term.screen_mut().install_line_read_layout(read, seqno);
            }
            // The request named the previous layout. Advertise the new state
            // before accepting coordinates from a refreshed client request.
            // The CurrentPane caller sends notify_lines_ready after this
            // false result; do not reacquire registration authority here.
            return Ok(false);
        }
        publish();
        Ok(true)
    }

    fn capture_surface_snapshot(
        &self,
        baseline: SequenceNo,
    ) -> Option<Result<PaneSurfaceSnapshot, frankenterm_term::screen::ColdReadMetadataBusy>> {
        let mut term = match self.terminal.try_lock() {
            Some(term) => term,
            None => return Some(Err(frankenterm_term::screen::ColdReadMetadataBusy)),
        };
        #[cfg(feature = "disruptor-pane-io")]
        self.drain_action_ring_locked(&mut term);

        match self.tmux_domain.try_lock() {
            Some(guard) if guard.is_some() => return None,
            Some(_) => {}
            None => return Some(Err(frankenterm_term::screen::ColdReadMetadataBusy)),
        }

        let line_layout_floor =
            match Self::refresh_line_layout_floor(&self.line_layout_observation, &mut term) {
                Ok(Some(floor)) => floor,
                // Let the existing source-fence path retire an exhausted
                // sequence domain instead of turning it into retryable busy.
                Ok(None) if term.current_seqno() == SequenceNo::MAX => return None,
                Ok(None) | Err(_) => {
                    return Some(Err(frankenterm_term::screen::ColdReadMetadataBusy))
                }
            };

        let dimensions = match terminal_try_get_dimensions(&mut term) {
            Some(dims) => dims,
            None => return Some(Err(frankenterm_term::screen::ColdReadMetadataBusy)),
        };

        let mouse_grabbed = term.is_mouse_grabbed();
        let alt_screen_active = term.is_alt_screen_active();

        let tiered_scrollback_status = match term.screen().try_tiered_scrollback_status() {
            Ok(status) => status.map(Into::into),
            Err(busy) => return Some(Err(busy)),
        };

        let cursor_position = terminal_get_cursor_position(&mut term);

        let source_sequence = term.current_seqno();

        let damage_baseline = crate::pane::changed_since_query_baseline(baseline, source_sequence);

        let viewport_range = match StableRowIndex::try_from(dimensions.viewport_rows)
            .ok()
            .and_then(|len| dimensions.physical_top.checked_add(len))
        {
            Some(end) => dimensions.physical_top..end,
            None => return Some(Err(frankenterm_term::screen::ColdReadMetadataBusy)),
        };

        let dirty_lines = terminal_get_dirty_lines(&mut term, viewport_range, damage_baseline);

        let screen = term.screen();

        let phys_start = match screen.stable_row_to_phys(dimensions.physical_top) {
            Some(phys) => phys,
            None => return Some(Err(frankenterm_term::screen::ColdReadMetadataBusy)),
        };
        let phys_end = match phys_start.checked_add(dimensions.viewport_rows) {
            Some(end) => end,
            None => return Some(Err(frankenterm_term::screen::ColdReadMetadataBusy)),
        };
        let lines = screen.lines_in_phys_range(phys_start..phys_end);
        if lines.len() != dimensions.viewport_rows {
            return Some(Err(frankenterm_term::screen::ColdReadMetadataBusy));
        }
        let viewport_lines = (dimensions.physical_top, lines);

        let cursor_phys = match screen.stable_row_to_phys(cursor_position.y) {
            Some(phys) => phys,
            None => return Some(Err(frankenterm_term::screen::ColdReadMetadataBusy)),
        };
        let cursor_end = match cursor_phys.checked_add(1) {
            Some(end) => end,
            None => return Some(Err(frankenterm_term::screen::ColdReadMetadataBusy)),
        };
        let cursor_row_lines = screen.lines_in_phys_range(cursor_phys..cursor_end);
        if cursor_row_lines.len() != 1 {
            return Some(Err(frankenterm_term::screen::ColdReadMetadataBusy));
        }
        let cursor_lines = (cursor_position.y, cursor_row_lines);

        let raw_title = term.get_title().to_string();
        let cached_cwd = term.get_current_dir().cloned();

        drop(term);

        let title = self.resolve_pane_title(raw_title);
        let working_dir =
            cached_cwd.or_else(|| self.divine_current_working_dir(CachePolicy::AllowStale));

        Some(Ok(PaneSurfaceSnapshot {
            source_sequence,
            line_layout_floor: Some(line_layout_floor),
            dimensions,
            mouse_grabbed,
            alt_screen_active,
            tiered_scrollback_status,
            cursor_position,
            title,
            working_dir,
            dirty_lines,
            viewport_lines,
            cursor_lines,
        }))
    }

    fn get_logical_lines(&self, lines: Range<StableRowIndex>) -> Vec<LogicalLine> {
        crate::pane::impl_get_logical_lines_via_get_lines(self, lines)
    }

    fn get_dimensions(&self) -> RenderableDimensions {
        terminal_get_dimensions(&mut self.locked_terminal())
    }

    fn get_tiered_scrollback_status(
        &self,
    ) -> Option<crate::renderable::PaneTieredScrollbackStatus> {
        Some(
            self.terminal
                .lock()
                .screen()
                .tiered_scrollback_status()
                .into(),
        )
    }

    fn copy_user_vars(&self) -> HashMap<String, String> {
        self.locked_terminal().user_vars().clone()
    }

    fn exit_behavior(&self) -> Option<ExitBehavior> {
        // If we are ssh, and we've not yet fully connected,
        // then override exit_behavior so that we can show
        // connection issues
        let mut pty = self.pty.lock();
        let is_ssh_connecting = pty
            .downcast_mut::<crate::ssh::WrappedSshPty>()
            .map(|s| s.is_connecting())
            .unwrap_or(false);
        let is_failed_spawn = pty.is::<crate::domain::FailedSpawnPty>();

        if is_ssh_connecting || is_failed_spawn {
            Some(ExitBehavior::CloseOnCleanExit)
        } else {
            None
        }
    }

    fn kill(&self) {
        if self.ownership.request_explicit_close(self.pane_id) {
            let mut proc = self.process.lock();
            log::debug!(
                "explicitly closing guardian-backed process in pane {}, state is {:?}",
                self.pane_id,
                proc
            );
            match &mut *proc {
                ProcessState::Running { killed, .. }
                | ProcessState::DeadPendingClose { killed } => *killed = true,
                ProcessState::Dead => {}
            }
            return;
        }

        let mut proc = self.process.lock();
        log::debug!(
            "killing process in pane {}, state is {:?}",
            self.pane_id,
            proc
        );
        match &mut *proc {
            ProcessState::Running {
                signaller, killed, ..
            } => {
                let _ = signaller.kill();
                *killed = true;
            }
            ProcessState::DeadPendingClose { killed } => {
                *killed = true;
            }
            _ => {}
        }
    }

    fn is_dead(&self) -> bool {
        // This is normally scheduled directly by the child waiter. Retrying
        // here also recovers if the main-thread scheduler rejected or cancelled
        // that first runnable.
        self.child_exit_prune.try_schedule();
        let mut proc = self.process.lock();

        const EXIT_BEHAVIOR: &str = "This message is shown because \
            \x1b]8;;https://wezterm.org/\
            config/lua/config/exit_behavior.html\
            \x1b\\exit_behavior\x1b]8;;\x1b\\";

        let mut terse = String::new();
        let mut brief = String::new();
        let mut trailer = String::new();
        let cmd = &self.command_description;

        match &mut *proc {
            ProcessState::Running {
                child_waiter,
                killed,
                ..
            } => {
                let status = match child_waiter.try_recv() {
                    Ok(Ok(s)) => Some(s),
                    Err(TryRecvError::Empty) => None,
                    _ => Some(ExitStatus::with_exit_code(1)),
                };

                if let Some(status) = status {
                    let success = match status.success() {
                        true => true,
                        false => configuration()
                            .clean_exit_codes
                            .contains(&status.exit_code()),
                    };

                    match (
                        self.exit_behavior()
                            .unwrap_or_else(|| configuration().exit_behavior),
                        success,
                        killed,
                    ) {
                        (ExitBehavior::Close, _, _) => *proc = ProcessState::Dead,
                        (ExitBehavior::CloseOnCleanExit, false, _) => {
                            brief = format!("⚠️  Process {cmd} didn't exit cleanly");
                            terse = format!("{status}.");
                            trailer = format!("{EXIT_BEHAVIOR}=\"CloseOnCleanExit\"");

                            *proc = ProcessState::DeadPendingClose { killed: false }
                        }
                        (ExitBehavior::CloseOnCleanExit, ..) => *proc = ProcessState::Dead,
                        (ExitBehavior::Hold, success, false) => {
                            trailer = format!("{EXIT_BEHAVIOR}=\"Hold\"");

                            if success {
                                brief = format!("👍 Process {cmd} completed.");
                                terse = "done".to_string();
                            } else {
                                brief = format!("⚠️  Process {cmd} didn't exit cleanly");
                                terse = format!("{status}");
                            }
                            *proc = ProcessState::DeadPendingClose { killed: false }
                        }
                        (ExitBehavior::Hold, _, true) => *proc = ProcessState::Dead,
                    }
                    log::debug!("child terminated, new state is {:?}", proc);
                }
            }
            ProcessState::DeadPendingClose { killed } => {
                if *killed {
                    *proc = ProcessState::Dead;
                    log::debug!("child state -> {:?}", proc);
                }
            }
            ProcessState::Dead => {}
        }

        let mut notify = None;
        if !terse.is_empty() {
            match configuration().exit_behavior_messaging {
                ExitBehaviorMessaging::Verbose => {
                    if terse == "done" {
                        notify = Some(format!("\r\n{brief}\r\n{trailer}"));
                    } else {
                        notify = Some(format!("\r\n{brief}\r\n{terse}\r\n{trailer}"));
                    }
                }
                ExitBehaviorMessaging::Brief => {
                    if terse == "done" {
                        notify = Some(format!("\r\n{brief}"));
                    } else {
                        notify = Some(format!("\r\n{brief}\r\n{terse}"));
                    }
                }
                ExitBehaviorMessaging::Terse => {
                    notify = Some(format!("\r\n[{terse}]"));
                }
                ExitBehaviorMessaging::None => {}
            }
        }

        if let Some(notify) = notify {
            if let Some(registration) = self.mux_registration.load() {
                emit_output_for_pane(registration, &notify);
            }
        }

        match &*proc {
            ProcessState::Running { .. } => false,
            ProcessState::DeadPendingClose { .. } => false,
            ProcessState::Dead => true,
        }
    }

    fn set_clipboard(&self, clipboard: &Arc<dyn Clipboard>) {
        self.locked_terminal().set_clipboard(clipboard);
    }

    fn mux_registration_slot(&self) -> &Arc<PaneRegistrationSlot> {
        &self.mux_registration
    }

    fn mux_registration_did_bind(&self, registration: PaneRegistrationHandle) {
        self.child_exit_prune.registration_bound(&registration);
        self.spawn_proc_list_prime(registration);
    }

    fn set_download_handler(&self, handler: &Arc<dyn DownloadHandler>) {
        self.locked_terminal().set_download_handler(handler);
    }

    fn set_config(&self, config: Arc<dyn TerminalConfiguration>) {
        let mut terminal = self.locked_terminal();
        let config = if let Some(existing) = terminal.get_config().scrollback_spill_sink() {
            if let Some(settings) = config.downcast_ref::<config::TermConfig>() {
                Arc::new(settings.for_scrollback_sink(existing)) as Arc<dyn TerminalConfiguration>
            } else if config
                .scrollback_spill_sink()
                .is_some_and(|sink| Arc::ptr_eq(&sink, &existing))
            {
                config
            } else {
                log::error!(
                    "refusing to detach pane {} scrollback authority during config replacement",
                    self.pane_id
                );
                return;
            }
        } else {
            config
        };
        let sink = config
            .scrollback_spill_sink()
            .filter(|sink| sink.requires_scrollback_flush());
        terminal.set_config(config);
        *self.scrollback_flush_sink.lock() = sink;
    }

    fn get_config(&self) -> Option<Arc<dyn TerminalConfiguration>> {
        Some(self.locked_terminal().get_config())
    }

    fn perform_actions(&self, actions: Vec<termwiz::escape::Action>) {
        let _output_application = self.output_application.lock();
        #[cfg(not(feature = "disruptor-pane-io"))]
        {
            // Default path: apply directly under the terminal mutex.
            self.terminal.lock().perform_actions(actions);
        }
        #[cfg(feature = "disruptor-pane-io")]
        {
            // ft-87qfi: lock-free SPSC staging — see `perform_actions_disruptor`.
            self.perform_actions_disruptor(actions);
        }
        // With the disruptor enabled this also drains the admitted ring before
        // backpressure, so queued rows cannot be stranded until later input.
        let sink = self.scrollback_flush_sink.lock().clone();
        if let Some(sink) = sink {
            drop(self.locked_terminal());
            self.drain_scrollback_outside_terminal(sink);
        }
    }

    fn capture_live_parser_checkpoint(
        &self,
        _authority: LiveParserCaptureAuthority,
        pending_actions: &mut Vec<Action>,
        ground: termwiz::escape::parser::RecoveryGroundBoundary<'_>,
        limits: TerminalCheckpointLimits,
    ) -> Result<RecoveryTerminalCheckpointV2, LiveParserPaneCaptureError> {
        // `locked_terminal` drains the optional disruptor ring before it returns.
        // Apply this parser's still-local actions and capture staged hot state
        // under the terminal lock, then release the lock immediately so cold-history
        // materialization and serialization do not block the terminal mutex.
        let staged = {
            let _output_application = self.output_application.lock();
            let mut terminal = self.locked_terminal();
            terminal.perform_actions(std::mem::take(pending_actions));
            terminal
                .capture_staged(limits)
                .map_err(LiveParserPaneCaptureError::Terminal)?
        };

        let checkpoint = staged
            .materialize_cold_history(limits)
            .map_err(RecoveryTerminalCheckpointError::Checkpoint)
            .map_err(LiveParserPaneCaptureError::Terminal)?;

        checkpoint
            .into_recovery_checkpoint_at_external_parser_ground(ground, limits)
            .map_err(RecoveryTerminalCheckpointError::Checkpoint)
            .map_err(LiveParserPaneCaptureError::Terminal)
    }

    fn mouse_event(&self, event: MouseEvent) -> Result<(), Error> {
        record_input_for_current_identity(&self.mux_registration);
        self.locked_terminal().mouse_event(event)
    }

    fn key_down(&self, key: KeyCode, mods: KeyModifiers) -> Result<(), Error> {
        record_input_for_current_identity(&self.mux_registration);
        if self.tmux_domain.lock().is_some() {
            log::trace!("key: {:?}", key);
            if key == KeyCode::Char('q') {
                self.locked_terminal().send_paste("detach\n")?;
            }
            return Ok(());
        } else {
            self.locked_terminal().key_down(key, mods)
        }
    }

    fn key_up(&self, key: KeyCode, mods: KeyModifiers) -> Result<(), Error> {
        record_input_for_current_identity(&self.mux_registration);
        self.locked_terminal().key_up(key, mods)
    }

    fn resize(&self, size: TerminalSize) -> Result<(), Error> {
        self.enqueue_resize(size, false)
    }

    fn resize_from_remote(&self, size: TerminalSize) -> Result<(), Error> {
        self.enqueue_resize(size, true)
    }

    fn writer(&self) -> MappedMutexGuard<'_, dyn std::io::Write> {
        record_input_for_current_identity(&self.mux_registration);
        MutexGuard::map(self.writer.lock(), |writer| {
            let w: &mut dyn std::io::Write = writer;
            w
        })
    }

    fn guardian_live_output_reader(
        &self,
    ) -> anyhow::Result<Option<Box<dyn GuardianLiveOutputReader>>> {
        Ok(self.guardian_live_output_reader.lock().take())
    }

    fn take_guardian_restored_prefix(
        &self,
    ) -> anyhow::Result<Option<crate::guardian_checkpoint::GuardianRestoredParserPrefix>> {
        let mut prefix = self.guardian_restored_prefix.lock();
        if let Some(prefix) = prefix.as_ref() {
            prefix.validate_model(
                &self.terminal.lock(),
                Uuid::from_bytes(self.durable_pane_id),
            )?;
        }
        Ok(prefix.take())
    }

    fn publish_guardian_checkpoint(
        &self,
        capture: LiveParserCheckpointAck,
    ) -> anyhow::Result<crate::guardian_checkpoint::PublishedGuardianCheckpoint> {
        self.guardian_checkpoint_publisher
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("pane does not own a guardian checkpoint publisher"))?
            .publish_checkpoint(capture)
    }

    fn reader(&self) -> anyhow::Result<Option<Box<dyn std::io::Read + Send>>> {
        Ok(Some(self.pty.lock().try_clone_reader()?))
    }

    fn send_paste(&self, text: &str) -> Result<(), Error> {
        record_input_for_current_identity(&self.mux_registration);
        if self.tmux_domain.lock().is_some() {
            Ok(())
        } else {
            self.locked_terminal().send_paste(text)
        }
    }

    fn get_title(&self) -> String {
        let title = self.locked_terminal().get_title().to_string();
        self.resolve_pane_title(title)
    }

    fn get_title_metadata(&self) -> PaneTitleMetadata {
        let mut is_stale = false;
        let snapshot = if let Some(term) = self.terminal.try_lock() {
            #[cfg(feature = "disruptor-pane-io")]
            let term = {
                let mut term = term;
                self.drain_action_ring_locked(&mut term);
                term
            };
            let snapshot = Arc::new(Self::capture_title_metadata(&term));
            // Publish while still holding the terminal guard so a delayed
            // reader cannot replace a newer capture with older metadata.
            let retired =
                std::mem::replace(&mut *self.title_metadata.lock(), Arc::clone(&snapshot));
            drop(term);
            drop(retired);
            snapshot
        } else {
            is_stale = true;
            Arc::clone(&self.title_metadata.lock())
        };
        let mut metadata = (*snapshot).clone();
        metadata.is_stale = is_stale;
        metadata.title = self.resolve_pane_title(metadata.title);
        metadata
    }

    fn get_progress(&self) -> Progress {
        self.locked_terminal().get_progress()
    }

    fn palette(&self) -> ColorPalette {
        self.locked_terminal().palette()
    }

    fn domain_id(&self) -> DomainId {
        self.domain_id
    }

    fn erase_scrollback(&self, erase_mode: ScrollbackEraseMode) {
        match erase_mode {
            ScrollbackEraseMode::ScrollbackOnly => {
                self.locked_terminal().erase_scrollback();
            }
            ScrollbackEraseMode::ScrollbackAndViewport => {
                self.locked_terminal().erase_scrollback_and_viewport();
            }
        }
    }

    fn focus_changed(&self, focused: bool) {
        self.locked_terminal().focus_changed(focused);
    }

    fn has_unseen_output(&self) -> bool {
        self.locked_terminal().has_unseen_output()
    }

    fn is_mouse_grabbed(&self) -> bool {
        if self.tmux_domain.lock().is_some() {
            false
        } else {
            self.locked_terminal().is_mouse_grabbed()
        }
    }

    fn is_alt_screen_active(&self) -> bool {
        if self.tmux_domain.lock().is_some() {
            false
        } else {
            self.locked_terminal().is_alt_screen_active()
        }
    }

    fn get_current_working_dir(&self, policy: CachePolicy) -> Option<Url> {
        self.terminal
            .lock()
            .get_current_dir()
            .cloned()
            .or_else(|| self.divine_current_working_dir(policy))
    }

    fn tty_name(&self) -> Option<String> {
        #[cfg(unix)]
        {
            let name = self.pty.lock().tty_name()?;
            Some(name.to_string_lossy().into_owned())
        }

        #[cfg(windows)]
        {
            None
        }
    }

    fn get_foreground_process_info(&self, policy: CachePolicy) -> Option<LocalProcessInfo> {
        #[cfg(unix)]
        if let Some(pid) = self.pty.lock().process_group_leader() {
            return LocalProcessInfo::with_root_pid(pid as u32);
        }

        self.divine_foreground_process(policy)
    }

    fn get_foreground_process_name(&self, policy: CachePolicy) -> Option<String> {
        #[cfg(unix)]
        {
            let leader = self.get_leader(policy);
            if let Some(path) = &leader.path {
                return Some(path.to_string_lossy().to_string());
            }
            return None;
        }

        #[cfg(windows)]
        if let Some(fg) = self.divine_foreground_process(policy) {
            return Some(fg.executable.to_string_lossy().to_string());
        }

        #[allow(unreachable_code)]
        None
    }

    fn can_close_without_prompting(&self, _reason: CloseReason) -> bool {
        // Fast path: read the proc_list cache without invoking the
        // O(N_system_processes) `proc_listallpids` walk that
        // `divine_process_list(FetchImmediate)` would trigger. On a host
        // with hundreds of processes (active agent swarm) the synchronous
        // walk routinely takes 1-2+ seconds and beach-balls the GUI on the
        // close-tab path. See ft-qhwpq.
        //
        // On cache miss we conservatively return false (user gets a
        // confirmation prompt — safe) and kick off a single-flight
        // background warm so the *next* close attempt has a fresh cache and
        // can render the no-prompt fast path.
        //
        // Inner Option is the memoized stateful decision: hit it directly
        // and we skip the synchronous `mux-is-process-stateful` Lua hook +
        // `default_stateful_check` HashSet build. The Lua hook still runs
        // on cold-decision attempts but is then memoized for the rest of
        // this cache TTL window.
        //
        // `entry_generation` is the cache entry's `updated: Instant`,
        // captured at read time. We use it below to detect whether the
        // warm worker raced ahead and replaced the entry between our read
        // and our write-back of the computed decision — if so, dropping
        // the write-back avoids labeling the new entry with a decision
        // computed from the old proc tree.
        let cached: Option<(LocalProcessInfo, Option<bool>, Instant)> = {
            let proc_list = self.proc_list.lock();
            proc_list.as_ref().and_then(|info| {
                if info.updated.elapsed() < PROC_INFO_CACHE_TTL {
                    Some((info.root.clone(), info.cached_is_stateful, info.updated))
                } else {
                    None
                }
            })
        };

        let (info_root, cached_decision, entry_generation) = match cached {
            Some(triple) => triple,
            None => {
                self.spawn_proc_list_warm();
                // Fallback: prefer a cheap process_group_leader probe so a
                // dead PTY can still close without prompting, matching the
                // previous behavior of the FetchImmediate-None branch.
                #[cfg(unix)]
                {
                    if self.pty.lock().process_group_leader().is_none() {
                        return true;
                    }
                }
                return false;
            }
        };

        // Hot path: previously decided. No Lua, no HashSet build, no clone
        // of `LocalProcessInfo` for the hook payload.
        if let Some(is_stateful) = cached_decision {
            return !is_stateful;
        }

        log::trace!(
            "can_close_without_prompting? procs in pane {:#?}",
            info_root
        );

        let hook_result = {
            #[cfg(feature = "lua")]
            {
                config::run_immediate_with_lua_config(|lua| {
                    let lua = match lua {
                        Some(lua) => lua,
                        None => return Ok(None),
                    };
                    let v = config::lua::emit_sync_callback(
                        &*lua,
                        ("mux-is-process-stateful".to_string(), (info_root.clone())),
                    )?;
                    match v {
                        mlua::Value::Nil => Ok(None),
                        mlua::Value::Boolean(v) => Ok(Some(v)),
                        _ => Ok(None),
                    }
                })
            }
            #[cfg(not(feature = "lua"))]
            {
                Ok::<Option<bool>, Error>(None)
            }
        };

        fn default_stateful_check(proc_list: &LocalProcessInfo) -> bool {
            // Fig uses `figterm` a pseudo terminal for a lot of functionality, it runs between
            // the shell and terminal. Unfortunately it is typically named `<shell> (figterm)`,
            // which prevents the statuful check from passing. This strips the suffix from the
            // process name to allow the check to pass.
            let names = proc_list
                .flatten_to_exe_names()
                .into_iter()
                .map(|s| match s.strip_suffix(" (figterm)") {
                    Some(s) => s.into(),
                    None => s,
                })
                .collect::<HashSet<_>>();

            let skip = configuration()
                .skip_close_confirmation_for_processes_named
                .iter()
                .cloned()
                .collect::<HashSet<_>>();

            if !names.is_subset(&skip) {
                // There are other processes running than are listed,
                // so we consider this to be stateful
                return true;
            }
            false
        }

        let is_stateful = match hook_result {
            Ok(None) => default_stateful_check(&info_root),
            Ok(Some(s)) => s,
            Err(err) => {
                log::error!(
                    "Error while running mux-is-process-stateful \
                     hook: {:#}, falling back to default behavior",
                    err
                );
                default_stateful_check(&info_root)
            }
        };

        // Memoize so other close attempts within the cache TTL skip the
        // Lua hook + HashSet build. Guarded against the cache having been
        // replaced by the warm worker between our read and write: the
        // generation check (`info.updated == entry_generation`) ensures we
        // only overwrite the entry we computed our decision against, never
        // a fresher entry whose proc tree is different.
        {
            let mut proc_list = self.proc_list.lock();
            if let Some(info) = proc_list.as_mut() {
                if info.updated == entry_generation {
                    info.cached_is_stateful = Some(is_stateful);
                }
            }
        }

        !is_stateful
    }

    fn get_semantic_zones(&self) -> anyhow::Result<Vec<SemanticZone>> {
        let mut term = self.locked_terminal();
        term.get_semantic_zones()
    }

    fn get_semantic_exit_code(&self) -> anyhow::Result<Option<i32>> {
        let term = self.locked_terminal();
        Ok(term.last_semantic_command_status())
    }

    async fn search(
        &self,
        pattern: Pattern,
        range: Range<StableRowIndex>,
        limit: Option<u32>,
    ) -> anyhow::Result<Vec<SearchResult>> {
        let term = self.locked_terminal();
        let screen = term.screen();

        enum CompiledPattern {
            CaseSensitiveString(String),
            CaseInSensitiveString(String),
            Regex(Regex),
        }

        let pattern = match pattern {
            Pattern::CaseSensitiveString(s) => CompiledPattern::CaseSensitiveString(s),
            Pattern::CaseInSensitiveString(s) => {
                // normalize the case so we match everything lowercase
                CompiledPattern::CaseInSensitiveString(s.to_lowercase())
            }
            Pattern::Regex(r) => CompiledPattern::Regex(Regex::new(&r)?),
        };

        let mut results = vec![];
        let mut uniq_matches: HashMap<String, usize> = HashMap::new();

        screen.for_each_logical_line_in_stable_range(range, |sr, lines| {
            if let Some(limit) = limit {
                if results.len() == limit as usize {
                    // We've reach the limit, stop iteration.
                    return false;
                }
            }

            if lines.is_empty() {
                // Nothing to do on this iteration, carry on with the next.
                return true;
            }
            let haystack = if lines.len() == 1 {
                lines[0].as_str()
            } else {
                let mut s = String::new();
                for line in lines {
                    s.push_str(&line.as_str());
                }
                Cow::Owned(s)
            };
            let stable_idx = sr.start;

            if haystack.is_empty() {
                return true;
            }

            let haystack = match &pattern {
                CompiledPattern::CaseInSensitiveString(_) => Cow::Owned(haystack.to_lowercase()),
                _ => haystack,
            };
            let mut coords = None;

            match &pattern {
                CompiledPattern::CaseInSensitiveString(s)
                | CompiledPattern::CaseSensitiveString(s) => {
                    for (idx, s) in haystack.match_indices(s) {
                        found_match(
                            s,
                            idx,
                            lines,
                            stable_idx,
                            &mut uniq_matches,
                            &mut coords,
                            &mut results,
                        );
                    }
                }
                CompiledPattern::Regex(re) => {
                    // Allow for the regex to contain captures
                    for capture_res in re.captures_iter(&*haystack) {
                        if let Ok(c) = capture_res {
                            // Look for the captures in reverse order, as index==0 is
                            // the whole matched string.  We can't just call
                            // `c.iter().rev()` as the capture iterator isn't double-ended.
                            for idx in (0..c.len()).rev() {
                                if let Some(m) = c.get(idx) {
                                    found_match(
                                        m.as_str(),
                                        m.start(),
                                        lines,
                                        stable_idx,
                                        &mut uniq_matches,
                                        &mut coords,
                                        &mut results,
                                    );
                                    break;
                                }
                            }
                        }
                    }
                }
            }

            // Keep iterating
            true
        });

        #[derive(Copy, Clone, Debug)]
        struct Coord {
            byte_idx: usize,
            grapheme_idx: usize,
            stable_row: StableRowIndex,
        }

        fn found_match(
            text: &str,
            byte_idx: usize,
            lines: &[&Line],
            stable_idx: StableRowIndex,
            uniq_matches: &mut HashMap<String, usize>,
            coords: &mut Option<Vec<Coord>>,
            results: &mut Vec<SearchResult>,
        ) {
            if coords.is_none() {
                coords.replace(make_coords(lines, stable_idx));
            }
            let Some(coords) = coords.as_ref() else {
                return;
            };
            if coords.is_empty() {
                return;
            }

            let match_id = match uniq_matches.get(text).copied() {
                Some(id) => id,
                None => {
                    let id = uniq_matches.len();
                    uniq_matches.insert(text.to_owned(), id);
                    id
                }
            };
            let (start_x, start_y) = haystack_idx_to_coord(byte_idx, coords);
            let (end_x, end_y) = haystack_idx_to_coord(byte_idx + text.len(), coords);
            results.push(SearchResult {
                start_x,
                start_y,
                end_x,
                end_y,
                match_id,
            });
        }

        fn make_coords(lines: &[&Line], stable_row: StableRowIndex) -> Vec<Coord> {
            let mut byte_idx = 0;
            let mut coords = vec![];

            for (row_idx, line) in lines.iter().enumerate() {
                let Ok(row_offset) = StableRowIndex::try_from(row_idx) else {
                    break;
                };
                let Some(stable_row) = stable_row.checked_add(row_offset) else {
                    break;
                };
                for cell in line.visible_cells() {
                    coords.push(Coord {
                        byte_idx,
                        grapheme_idx: cell.cell_index(),
                        stable_row,
                    });
                    byte_idx += cell.str().len();
                }
            }

            coords
        }

        fn haystack_idx_to_coord(idx: usize, coords: &[Coord]) -> (usize, StableRowIndex) {
            let c = match coords.binary_search_by(|ele| ele.byte_idx.cmp(&idx)) {
                Ok(index) | Err(index) => index,
            };
            let coord = coords.get(c).map(|c| *c).unwrap_or_else(|| {
                let Some(last) = coords.last() else {
                    return Coord {
                        byte_idx: 0,
                        grapheme_idx: 0,
                        stable_row: 0,
                    };
                };
                Coord {
                    grapheme_idx: next_search_grapheme_idx(last.grapheme_idx),
                    ..*last
                }
            });
            (coord.grapheme_idx, coord.stable_row)
        }

        Ok(results)
    }
}

struct LocalPaneDCSHandler {
    pane_id: PaneId,
    tmux_domain: Arc<Mutex<Option<Arc<TmuxDomainState>>>>,
    mux_registration: Arc<PaneRegistrationSlot>,
}

const MAX_GENERATED_OUTPUT_WORKERS: usize = 16;
const MAX_GENERATED_OUTPUT_MESSAGE_BYTES: usize = 64 * 1024;
static GENERATED_OUTPUT_WORKERS: AtomicUsize = AtomicUsize::new(0);

struct GeneratedOutputPermit(&'static AtomicUsize);

impl Drop for GeneratedOutputPermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Generated exit/control-mode notices can arrive while the caller holds the
/// process or terminal lock, including on the GUI thread. Never apply or drain
/// them inline. This bounded auxiliary lane does not carry PTY output bytes.
fn spawn_generated_output(
    workers: &'static AtomicUsize,
    message: &str,
    apply: impl FnOnce(Vec<Action>) + Send + 'static,
) -> bool {
    spawn_auxiliary_output(workers, message.len(), || {
        let message = message.to_owned();
        move || {
            let mut parser = termwiz::escape::parser::Parser::new();
            let mut actions = vec![Action::CSI(CSI::Sgr(Sgr::Reset))];
            parser.parse(message.as_bytes(), |action| actions.push(action));
            apply(actions);
        }
    })
}

fn spawn_auxiliary_output<F: FnOnce() + Send + 'static>(
    workers: &'static AtomicUsize,
    estimated_bytes: usize,
    make_apply: impl FnOnce() -> F,
) -> bool {
    if estimated_bytes > MAX_GENERATED_OUTPUT_MESSAGE_BYTES
        || workers
            .try_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < MAX_GENERATED_OUTPUT_WORKERS).then(|| active + 1)
            })
            .is_err()
    {
        metrics::counter!("mux.generated_output.rejected").increment(1);
        log::error!("generated pane notice rejected by bounded worker admission");
        return false;
    }
    let permit = GeneratedOutputPermit(workers);
    let apply = make_apply();
    let spawned = std::thread::Builder::new()
        .name("mux-generated-output".to_string())
        .spawn(move || {
            let _permit = permit;
            if catch_recoverable(
                RecoverablePanicSite::MuxPaneCallback,
                AssertUnwindSafe(apply),
            )
            .is_err()
            {
                metrics::counter!("mux.generated_output.panicked").increment(1);
                log::error!("generated pane notice worker failed at the pane callback boundary");
            }
        });
    if let Err(error) = spawned {
        // A failed spawn drops its closure, returning the permit as well.
        metrics::counter!("mux.generated_output.spawn_failed").increment(1);
        log::error!("cannot spawn generated pane notice worker: {error}");
        return false;
    }
    true
}

/// Fixed-size GUI controls share bounded admission with generated notices,
/// but must not inherit the notice formatter's implicit SGR reset.
pub enum PaneControlAction {
    Reset,
    Bell,
}

pub fn schedule_control_action(
    registration: PaneRegistrationHandle,
    control: PaneControlAction,
) -> anyhow::Result<()> {
    let action = match control {
        PaneControlAction::Reset => Action::Esc(termwiz::escape::Esc::Code(
            termwiz::escape::EscCode::FullReset,
        )),
        PaneControlAction::Bell => Action::Control(termwiz::escape::ControlCode::Bell),
    };
    anyhow::ensure!(
        spawn_auxiliary_output(
            &GENERATED_OUTPUT_WORKERS,
            std::mem::size_of::<Action>(),
            || move || {
                let _ = registration.try_with_current_output(|pane| {
                    pane.perform_actions(vec![action]);
                });
            }
        ),
        "terminal control could not be queued; background output capacity is unavailable"
    );
    Ok(())
}

pub(crate) fn emit_output_for_pane(registration: PaneRegistrationHandle, message: &str) {
    spawn_generated_output(&GENERATED_OUTPUT_WORKERS, message, move |actions| {
        let _ = registration.try_with_current_output(|pane| {
            pane.perform_actions(actions);
        });
    });
}

impl frankenterm_term::DeviceControlHandler for LocalPaneDCSHandler {
    fn handle_device_control(&mut self, control: termwiz::escape::DeviceControlMode) {
        match control {
            DeviceControlMode::Enter(mode) => {
                if !mode.ignored_extra_intermediates
                    && mode.params.len() == 1
                    && mode.params[0] == 1000
                    && mode.intermediates.is_empty()
                {
                    log::info!("tmux -CC mode requested");

                    // Create a new domain to host these tmux tabs
                    let domain = match TmuxDomain::new(self.pane_id) {
                        Ok(domain) => domain,
                        Err(err) => {
                            log::error!(
                                "cannot initialize tmux control-mode domain for pane {}: {err:#}",
                                self.pane_id
                            );
                            return;
                        }
                    };
                    let tmux_domain = Arc::clone(&domain.inner);

                    let domain: Arc<dyn Domain> = Arc::new(domain);
                    let Some(registration) = self.mux_registration.load() else {
                        log::warn!(
                            "ignoring tmux control mode request for unregistered pane {}",
                            self.pane_id
                        );
                        return;
                    };
                    let binding = Arc::clone(&tmux_domain);
                    let Some(result) = registration.try_with_current(
                        |pane| -> Result<(), crate::DomainRegistrationError> {
                            pane.register_domain(&domain)?;
                            self.tmux_domain.lock().replace(binding);
                            Ok(())
                        },
                    ) else {
                        log::warn!(
                            "ignoring tmux control mode request for stale pane registration {}",
                            self.pane_id
                        );
                        return;
                    };
                    if let Err(err) = result {
                        log::error!(
                            "cannot register tmux control-mode domain for pane {}: {err}",
                            self.pane_id
                        );
                        return;
                    }
                    // Close the narrow race where the supervisor starts
                    // successfully but panics between construction and
                    // registration. Its panic handler marks the domain
                    // terminal; once the exact domain and launcher binding are
                    // registered, this retry makes terminal cleanup
                    // authoritative instead of leaving a detached binding.
                    if tmux_domain.is_terminal() {
                        log::error!(
                            "tmux control-mode domain for pane {} lost its I/O supervisor during \
                             registration",
                            self.pane_id
                        );
                        if let Err(err) = domain.detach() {
                            log::error!(
                                "cannot finalize failed tmux control-mode domain for pane {}: \
                                 {err:#}",
                                self.pane_id
                            );
                        }
                        return;
                    }
                    emit_output_for_pane(
                        registration,
                        "\r\n[This pane is running tmux control mode. Press q to detach]",
                    );

                    // Initial tmux enumeration is driven by control-mode events:
                    // SessionChanged -> ListCommands -> ListAllWindows ->
                    // ListAllPanes -> AttachDone. Keep attach() as a no-op
                    // unless that bootstrap flow changes.
                } else if configuration().log_unknown_escape_sequences {
                    log::warn!("unknown DeviceControlMode::Enter {:?}", mode,);
                }
            }
            DeviceControlMode::Exit => {
                let tmux = self.tmux_domain.lock().take();
                if let Some(tmux) = tmux {
                    tmux.transition_to_clean_exit();
                }
            }
            DeviceControlMode::Data(c) => {
                if configuration().log_unknown_escape_sequences {
                    log::warn!(
                        "unhandled DeviceControlMode::Data {:x} {}",
                        c,
                        (c as char).escape_debug()
                    );
                }
            }
            DeviceControlMode::TmuxEvents(events) => {
                let tmux = self.tmux_domain.lock().clone();
                if let Some(tmux) = tmux {
                    tmux.advance(events);
                } else {
                    log::warn!("unhandled DeviceControlMode::TmuxEvents {:?}", events);
                }
            }
            _ => {
                if configuration().log_unknown_escape_sequences {
                    log::warn!("unhandled: {:?}", control);
                }
            }
        }
    }
}

struct LocalPaneNotifHandler {
    pane_id: PaneId,
    mux_registration: Arc<PaneRegistrationSlot>,
}

impl AlertHandler for LocalPaneNotifHandler {
    fn alert(&mut self, alert: Alert) {
        let Some(registration) = self.mux_registration.load() else {
            log::trace!(
                "dropping alert for unregistered local pane {}",
                self.pane_id
            );
            return;
        };
        if !promise::spawn::is_scheduler_configured() {
            let _ = registration.try_with_current(|pane| {
                pane.dispatch_alert(alert);
            });
            return;
        }
        match promise::spawn::try_reserve_main_thread(
            promise::spawn::MainThreadServiceClass::Interactive,
            LOCAL_PANE_MAIN_THREAD_ESTIMATED_BYTES,
        ) {
            promise::spawn::MainThreadReservationOutcome::Reserved(reservation) => {
                reservation
                    .spawn(async move {
                        let _ = registration.try_with_current(|pane| {
                            pane.dispatch_alert(alert);
                        });
                    })
                    .detach();
            }
            rejected => {
                metrics::counter!(
                    "mux.local_pane.main_thread_admission",
                    "operation" => "pane alert",
                    "outcome" => "inline_fallback"
                )
                .increment(1);
                log::error!(
                    "main-thread scheduler rejected local-pane alert; preserving alert inline: {rejected:?}"
                );
                let _ = registration.try_with_current(|pane| {
                    pane.dispatch_alert(alert);
                });
            }
        }
    }
}

/// This is a little gross; on some systems, our pipe reader will continue
/// to be blocked in read even after the child process has died.
/// We need to wake up and notice that the child terminated in order
/// for our state to wind down.
/// This block schedules a background thread to wait for the child
/// to terminate, and then nudge the muxer to check for dead processes.
/// Without this, typing `exit` in `cmd.exe` would keep the pane around
/// until something else triggered the mux to prune dead processes.
fn split_child(
    mut process: Box<dyn Child>,
    child_exit_prune: Arc<ChildExitPruneState>,
) -> (
    Receiver<IoResult<ExitStatus>>,
    Box<dyn ChildKiller + Sync>,
    Option<u32>,
) {
    let pid = process.process_id();
    let signaller = process.clone_killer();

    let (tx, rx) = sync_channel(1);
    let waiter_tx = tx.clone();
    let thread_name = pid
        .map(|pid| format!("pane-child-waiter-{pid}"))
        .unwrap_or_else(|| "pane-child-waiter".to_string());
    let waiter_prune = Arc::clone(&child_exit_prune);

    let spawn_result = std::thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            let status = process.wait();
            waiter_tx.send(status).ok();
            waiter_prune.mark_child_exited();
        });

    if let Err(err) = spawn_result {
        log::error!("failed to spawn child waiter thread pid={pid:?} error={err:#}");
        tx.send(Err(err)).ok();
        child_exit_prune.mark_child_exited();
    }

    (rx, signaller, pid)
}

impl LocalPane {
    /// Capture a legacy mux-owned pane terminal checkpoint using distinct model-only authority.
    ///
    /// The capture separates hot-state capture from cold-history materialization:
    /// 1. Under terminal lock: drain/verify pending actions, capture bounded hot state, and pin
    ///    cold-history generation from the scrollback spill sink.
    /// 2. Release terminal lock immediately.
    /// 3. Outside terminal lock: materialize cold-history rows from the spill sink, revalidate
    ///    generation freshness, and assemble canonical `RecoveryTerminalCheckpointV2`.
    pub(crate) fn current_model_semantic_generation(&self) -> Option<u64> {
        if matches!(self.ownership, LocalPaneOwnership::Guardian(_)) {
            return None;
        }
        let terminal = self.terminal.try_lock()?;
        let seqno = terminal.current_seqno();
        if seqno == usize::MAX {
            return None;
        }
        u64::try_from(seqno).ok()
    }

    pub(crate) fn current_guardian_semantic_generation(&self) -> Option<u64> {
        if !matches!(self.ownership, LocalPaneOwnership::Guardian(_)) {
            return None;
        }
        let terminal = self.terminal.try_lock()?;
        let seqno = terminal.current_seqno();
        if seqno == usize::MAX {
            return None;
        }
        u64::try_from(seqno).ok()
    }

    pub(crate) fn guardian_lease_identity(&self) -> Option<GuardianPaneLeaseIdentity> {
        match &self.ownership {
            LocalPaneOwnership::Guardian(guardian) => Some(guardian.identity),
            LocalPaneOwnership::LegacyMuxOwned => None,
        }
    }

    pub(crate) fn stage_legacy_terminal_checkpoint(
        &self,
        _authority: ModelParserCaptureAuthority,
        pending_actions: &mut Vec<Action>,
        ground: termwiz::escape::parser::RecoveryGroundBoundary<'_>,
        limits: TerminalCheckpointLimits,
    ) -> Result<
        frankenterm_term::terminalstate::checkpoint::StagedRecoveryCheckpoint,
        LegacyTerminalCaptureError,
    > {
        if matches!(self.ownership, LocalPaneOwnership::Guardian(_)) {
            return Err(LegacyTerminalCaptureError::FalseGuardianAuthority);
        }
        let _output_application = self.output_application.lock();
        let mut terminal = self.locked_terminal();
        if !pending_actions.is_empty() {
            terminal.perform_actions(std::mem::take(pending_actions));
        }
        let staged = terminal.capture_staged(limits)?;
        Ok(staged.bind_external_parser_ground(ground))
    }

    pub fn capture_legacy_terminal_checkpoint(
        &self,
        authority: ModelParserCaptureAuthority,
        pending_actions: &mut Vec<Action>,
        ground: termwiz::escape::parser::RecoveryGroundBoundary<'_>,
        limits: TerminalCheckpointLimits,
    ) -> Result<RecoveryTerminalCheckpointV2, LegacyTerminalCaptureError> {
        self.capture_legacy_terminal_checkpoint_with_policy(
            authority,
            pending_actions,
            ground,
            limits,
            PendingActionDrainPolicy::DrainAndApply,
        )
    }

    /// Capture legacy terminal checkpoint with explicit pending-action handling policy.
    pub fn capture_legacy_terminal_checkpoint_with_policy(
        &self,
        _authority: ModelParserCaptureAuthority,
        pending_actions: &mut Vec<Action>,
        ground: termwiz::escape::parser::RecoveryGroundBoundary<'_>,
        limits: TerminalCheckpointLimits,
        policy: PendingActionDrainPolicy,
    ) -> Result<RecoveryTerminalCheckpointV2, LegacyTerminalCaptureError> {
        // Enforce distinct model-only authority: fail closed if guardian-owned
        if matches!(self.ownership, LocalPaneOwnership::Guardian(_)) {
            return Err(LegacyTerminalCaptureError::FalseGuardianAuthority);
        }

        match policy {
            PendingActionDrainPolicy::RequireEmpty => {
                if !pending_actions.is_empty() {
                    return Err(LegacyTerminalCaptureError::PendingActionsRemain(
                        pending_actions.len(),
                    ));
                }
            }
            PendingActionDrainPolicy::DrainAndApply => {}
        }

        // 1. Under terminal lock: drain/apply actions, capture hot state, pin cold generation
        let staged = {
            let _output_application = self.output_application.lock();
            let mut terminal = self.locked_terminal();
            if policy == PendingActionDrainPolicy::DrainAndApply {
                terminal.perform_actions(std::mem::take(pending_actions));
            }
            terminal.capture_staged(limits).map_err(|e| match e {
                RecoveryTerminalCheckpointError::Checkpoint(
                    TerminalCheckpointError::StaleColdGeneration,
                ) => LegacyTerminalCaptureError::StaleColdGeneration,
                other => LegacyTerminalCaptureError::Terminal(other),
            })?
        }; // Terminal mutex is released immediately here!

        // 2. Outside terminal lock: materialize cold history rows
        let checkpoint = staged
            .materialize_cold_history(limits)
            .map_err(|e| match e {
                TerminalCheckpointError::StaleColdGeneration => {
                    LegacyTerminalCaptureError::StaleColdGeneration
                }
                other => LegacyTerminalCaptureError::Terminal(
                    RecoveryTerminalCheckpointError::Checkpoint(other),
                ),
            })?;

        // 3. Serialize canonical payload and assemble RecoveryTerminalCheckpointV2 outside terminal lock
        checkpoint
            .into_recovery_checkpoint_at_external_parser_ground(ground, limits)
            .map_err(|e| match e {
                TerminalCheckpointError::StaleColdGeneration => {
                    LegacyTerminalCaptureError::StaleColdGeneration
                }
                other => LegacyTerminalCaptureError::Terminal(
                    RecoveryTerminalCheckpointError::Checkpoint(other),
                ),
            })
    }

    fn capture_title_metadata(terminal: &Terminal) -> PaneTitleMetadata {
        PaneTitleMetadata {
            is_stale: false,
            title: terminal.get_title().to_string(),
            user_vars: terminal.user_vars().clone(),
            progress: terminal.get_progress(),
            has_unseen_output: terminal.has_unseen_output(),
        }
    }

    fn resolve_pane_title(&self, title: String) -> String {
        // Preserve the process-name fallback for the legacy default title.
        if title == "wezterm" {
            if let Some(proc_name) = self.get_foreground_process_name(CachePolicy::AllowStale) {
                if let Some(name) = std::path::Path::new(&proc_name).file_name() {
                    return name.to_string_lossy().to_string();
                }
            }
        }
        title
    }

    fn refresh_line_layout_floor(
        observation: &LineLayoutObservation,
        term: &mut Terminal,
    ) -> Result<Option<SequenceNo>, frankenterm_term::screen::ColdReadMetadataBusy> {
        let mut observation = observation
            .try_lock()
            .ok_or(frankenterm_term::screen::ColdReadMetadataBusy)?;
        let Some(source_changed) = term.screen_mut().refresh_cold_source_observation()? else {
            return Ok(None);
        };
        let screen_changed = observation
            .as_ref()
            .is_some_and(|(witness, _)| !term.screen().matches_coordinate_witness(witness));
        if source_changed || screen_changed {
            term.increment_seqno();
        }
        if term.current_seqno() == SequenceNo::MAX {
            return Ok(None);
        }
        let floor = if source_changed || screen_changed {
            term.current_seqno()
        } else {
            observation
                .as_ref()
                .map_or(term.current_seqno(), |(_, floor)| *floor)
        }
        .max(term.screen().cold_visual_layout_seqno());
        *observation = Some((term.screen().capture_coordinate_witness(), floor));
        Ok(Some(floor))
    }

    /// Finalize the same layout authority used by frame capture before exposing
    /// a resize source. The caller holds both terminal and resize-intent guards;
    /// otherwise the first reader could advance a stale coordinate observation
    /// after the receipt and make the actual frame impossible to bind to it.
    fn publish_resize_source(
        pane_id: PaneId,
        observation: &LineLayoutObservation,
        term: &mut Terminal,
        token: ResizeCancellationToken,
        commit_id: u64,
        phase: &'static str,
    ) -> bool {
        let resize_sequence = term.current_seqno();
        if Self::refresh_line_layout_floor(observation, term)
            .ok()
            .flatten()
            .is_none()
        {
            // Busy storage/observation and saturated sequences are not proof of
            // an unchanged source. Keep the resize but publish no false receipt.
            metrics::counter!(
                "mux.localpane.resize.source_unavailable",
                "phase" => phase,
            )
            .increment(1);
            return false;
        }
        if phase == "primary" {
            let published_sequence = term.current_seqno();
            term.screen_mut()
                .publish_selection_anchor_sequence(resize_sequence, published_sequence);
        }
        let size = term.get_size();
        log::trace!(
            "LocalPane::resize committed_source pane_id={} seq={} commit_id={} source_sequence={} target={}x{} phase={}",
            pane_id,
            token.seq,
            commit_id,
            term.current_seqno(),
            size.cols,
            size.rows,
            phase,
        );
        true
    }

    /// One nonblocking observation for a GUI frame's coordinate authority.
    /// Do not split this into blocking sequence/dimension getters on the UI.
    pub fn selection_source_snapshot(
        &self,
    ) -> Option<(SequenceNo, SequenceNo, RenderableDimensions)> {
        let mut term = self.terminal.try_lock()?;
        let floor = Self::refresh_line_layout_floor(&self.line_layout_observation, &mut term)
            .ok()
            .flatten()?;
        let dimensions = terminal_try_get_dimensions(&mut term)?;
        Some((floor, term.current_seqno(), dimensions))
    }

    /// The source/layout checks and coordinate registration share one
    /// nonblocking terminal acquisition. A busy capture never fabricates a
    /// token from independently sampled metadata.
    pub fn capture_selection_anchor(
        &self,
        expected_floor: SequenceNo,
        expected_sequence: SequenceNo,
        expected_dimensions: RenderableDimensions,
        points: [Option<frankenterm_term::screen::SelectionAnchorCoordinate>; 3],
    ) -> Result<Option<frankenterm_term::screen::ScreenSelectionAnchor>, SelectionAnchorCaptureError>
    {
        let mut term = self
            .terminal
            .try_lock()
            .ok_or(SelectionAnchorCaptureError::Busy)?;
        let floor = Self::refresh_line_layout_floor(&self.line_layout_observation, &mut term)
            .ok()
            .flatten()
            .ok_or(SelectionAnchorCaptureError::Busy)?;
        let dimensions =
            terminal_try_get_dimensions(&mut term).ok_or(SelectionAnchorCaptureError::Busy)?;
        if floor != expected_floor || dimensions != expected_dimensions {
            return Err(SelectionAnchorCaptureError::SourceChanged);
        }
        let sequence = term.current_seqno();
        if sequence != expected_sequence {
            if sequence < expected_sequence {
                return Err(SelectionAnchorCaptureError::SourceChanged);
            }
            if !term
                .screen()
                .selection_anchor_rows_unchanged_since(&points, expected_sequence)
            {
                return Err(SelectionAnchorCaptureError::SourceChanged);
            }
        }
        // A valid source may not support remapping (alternate screen, cold
        // coordinates, or a full anchor registry). Ordinary selection remains
        // valid there; callers must not confuse this with a busy acquisition.
        Ok(term.screen_mut().capture_selection_anchor(sequence, points))
    }

    /// Outer None means unavailable; an inner None is an invalid token at the
    /// returned exact source. The GUI must distinguish those outcomes so a
    /// contended terminal does not destroy an otherwise valid drag.
    pub fn selection_anchor_snapshot(
        &self,
        anchor: &frankenterm_term::screen::ScreenSelectionAnchor,
    ) -> Option<(
        SequenceNo,
        SequenceNo,
        RenderableDimensions,
        Option<[Option<frankenterm_term::screen::SelectionAnchorCoordinate>; 3]>,
    )> {
        let mut term = self.terminal.try_lock()?;
        let floor = Self::refresh_line_layout_floor(&self.line_layout_observation, &mut term)
            .ok()
            .flatten()?;
        let dimensions = terminal_try_get_dimensions(&mut term)?;
        let sequence = term.current_seqno();
        let points = term.screen().resolve_selection_anchor(anchor, sequence);
        Some((floor, sequence, dimensions, points))
    }

    pub fn try_capture_render_frame(
        &self,
        viewport: Option<StableRowIndex>,
        damage_baseline: SequenceNo,
        selection_baseline: SequenceNo,
        rules: &[termwiz::hyperlink::Rule],
        detect_password_input: bool,
    ) -> Option<NativeRenderFrame> {
        let mut term = self.terminal.try_lock()?;
        #[cfg(feature = "disruptor-pane-io")]
        self.drain_action_ring_locked(&mut term);
        let hide_cursor = self.tmux_domain.try_lock()?.is_some();
        #[allow(unused_mut)]
        let mut password_input = false;
        #[cfg(unix)]
        if detect_password_input {
            use nix::sys::termios::LocalFlags;
            if let Some(tio) = self.pty.try_lock()?.get_termios() {
                password_input = !tio.local_flags.contains(LocalFlags::ECHO)
                    && tio.local_flags.contains(LocalFlags::ICANON);
            }
        }
        #[cfg(not(unix))]
        let _ = detect_password_input;
        let layout_floor =
            Self::refresh_line_layout_floor(&self.line_layout_observation, &mut term)
                .ok()
                .flatten()?;
        let dimensions = terminal_try_get_dimensions(&mut term)?;
        // Scrollback eviction and clearing can invalidate a stored viewport
        // without a GUI scroll event. Normalize against this same observation;
        // requesting an evicted row would otherwise retry hydration forever.
        let first = viewport
            .unwrap_or(dimensions.physical_top)
            .max(dimensions.scrollback_top)
            .min(dimensions.physical_top);
        let end = first.checked_add(StableRowIndex::try_from(dimensions.viewport_rows).ok()?)?;
        let range = first..end;
        let mut cursor = terminal_get_cursor_position(&mut term);
        if hide_cursor {
            cursor.visibility = termwiz::surface::CursorVisibility::Hidden;
        }
        let mut frame = NativeRenderFrame {
            layout_floor,
            source_sequence: term.current_seqno(),
            dimensions,
            cursor,
            palette: term.palette(),
            dirty: RangeSet::new(),
            selection_dirty: RangeSet::new(),
            first,
            lines: Vec::with_capacity(dimensions.viewport_rows),
            password_input,
            coordinate_witness: term.screen().capture_coordinate_witness(),
        };
        let cold = first < term.screen().phys_to_stable_row_index(0);
        if cold {
            // Cold hydration has its own worker/cache and must run outside the
            // terminal lock. Revalidate the complete source after that read;
            // a coordinate witness alone does not detect content-only edits.
            drop(term);
            self.with_lines_mut_and_apply_hyperlinks(range.clone(), rules, &mut frame);
            term = self.terminal.try_lock()?;
            if term.current_seqno() != frame.source_sequence
                || !term
                    .screen()
                    .matches_coordinate_witness(&frame.coordinate_witness)
            {
                return None;
            }
        } else {
            terminal_with_lines_mut_and_apply_hyperlinks(
                &mut term,
                range.clone(),
                rules,
                &mut frame,
            );
        }
        frame.source_sequence = term.current_seqno();
        let damage_baseline =
            crate::pane::changed_since_query_baseline(damage_baseline, frame.source_sequence);
        let selection_baseline =
            crate::pane::changed_since_query_baseline(selection_baseline, frame.source_sequence);
        frame.dirty = terminal_get_dirty_lines(&mut term, range.clone(), damage_baseline);
        frame.selection_dirty = terminal_get_dirty_lines(&mut term, range, selection_baseline);
        if frame.first != first || frame.lines.len() != dimensions.viewport_rows {
            // A cold cache miss has already scheduled hydration. Preserve the
            // previously presented frame instead of settling its damage with
            // a partial or empty replacement.
            return None;
        }
        drop(term);
        if !cold {
            if let Some(pending) = self
                .cold_viewport_pending
                .try_lock()
                .and_then(|mut pending| pending.take())
            {
                pending.cancelled.store(true, Ordering::Release);
            }
        }
        Some(frame)
    }

    /// Optional renderer-cache writeback, guarded by unchanged coordinates and
    /// exact row equality. A busy parser never delays an already drawn frame.
    pub fn publish_render_frame_appdata(&self, frame: &NativeRenderFrame) {
        let Some(end) = StableRowIndex::try_from(frame.lines.len())
            .ok()
            .and_then(|len| frame.first.checked_add(len))
        else {
            return;
        };
        let Some(mut term) = self.terminal.try_lock() else {
            return;
        };
        let screen = term.screen_mut();
        if !screen.matches_coordinate_witness(&frame.coordinate_witness) {
            return;
        }
        let physical = screen.stable_range(&(frame.first..end));
        if screen.phys_to_stable_row_index(physical.start) != frame.first {
            return;
        }
        screen.with_phys_lines(physical, |current| {
            for (current, rendered) in current.iter().zip(&frame.lines) {
                if *current == rendered {
                    current.copy_appdata_from(rendered);
                }
            }
        });
    }

    fn cold_viewport_lines(&self, requested: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
        let empty = || (requested.start, Vec::new());
        if requested.end.saturating_sub(requested.start).max(0) as usize
            > frankenterm_term::screen::ScreenLineRead::MAX_ROWS
        {
            return empty();
        }
        let Some(registration) = self.mux_registration.load() else {
            return empty();
        };
        if let Some(failure) = self.cold_viewport_failure.try_lock() {
            if let Some(failure) = failure.as_ref() {
                if failure.requested == requested {
                    if let Some(term) = self.terminal.try_lock() {
                        if failure.witness.matches(term.screen()) {
                            return empty();
                        }
                    }
                }
            }
        }
        let cached = COLD_VIEWPORT_CACHE.try_lock().and_then(|cache| {
            cache
                .iter()
                .find(|entry| {
                    entry.registration == registration.wire_identity()
                        && (entry.requested == requested
                            || entry.read.cached_lines(requested.clone()).is_some())
                })
                .map(|entry| Arc::clone(&entry.read))
        });
        if let Some(read) = cached {
            let mut snapshot = None;
            let _ = registration.try_with_current(|pane| {
                let _ = pane.publish_line_reads(std::slice::from_ref(read.as_ref()), &mut || {
                    let mut bytes_left =
                        frankenterm_term::screen::ScreenLineRead::MAX_PAYLOAD_BYTES;
                    let mut work_left = 65_536;
                    snapshot = read.try_clone_viewport_for_snapshot(
                        requested.clone(),
                        &mut bytes_left,
                        &mut work_left,
                    );
                });
            });
            if let Some(snapshot) = snapshot {
                return snapshot;
            }
        }
        let Some(mut pending) = self.cold_viewport_pending.try_lock() else {
            retry_cold_viewport(registration, Arc::clone(&self.cold_viewport_retry));
            return empty();
        };
        if pending
            .as_ref()
            .is_some_and(|pending| pending.requested == requested)
        {
            return empty();
        }
        if let Some(previous) = pending.take() {
            previous.cancelled.store(true, Ordering::Release);
        }
        let Some(permit) = crate::pane::LineReadPermit::try_acquire() else {
            retry_cold_viewport(registration, Arc::clone(&self.cold_viewport_retry));
            return empty();
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        let captured_witness = Arc::new(Mutex::new(None));
        let worker_witness = Arc::clone(&captured_witness);
        let failure_state = Arc::clone(&self.cold_viewport_failure);
        let terminal_for_failure = Arc::downgrade(&self.terminal);
        *pending = Some(ColdViewportPending {
            requested: requested.clone(),
            cancelled: Arc::clone(&cancelled),
        });
        drop(pending);
        let completion = ColdViewportCompletion {
            state: Arc::clone(&self.cold_viewport_pending),
            cancelled: Arc::clone(&cancelled),
        };
        let response_range = requested.clone();
        let retry = Arc::clone(&self.cold_viewport_retry);
        let capture_registration = registration.clone();
        let capture_retry = Arc::clone(&retry);
        let worker_cancelled = Arc::clone(&cancelled);
        let worker = permit.start(move || worker_cancelled.load(Ordering::Acquire), move |result, permit| {
            let mut plans = match result {
                Ok(plans) => plans,
                Err(error) => {
                    let Some(failure_witness): Option<frankenterm_term::screen::LineReadFailureWitness> = worker_witness.lock().take() else { return; };
                    metrics::counter!("mux.local_pane.cold_viewport", "outcome" => "read_rejected").increment(1);
                    if !completion.cancelled.load(Ordering::Acquire) {
                        if failure_witness.retry_without_index() {
                            // Retry once as a separately admitted bounded
                            // viewport read; do not turn an optional full-index
                            // budget refusal into permanent missing history.
                            retry_cold_viewport(registration, retry);
                            return;
                        }
                        let geometry = error.is::<frankenterm_term::screen::ColdReadGeometryUnavailable>();
                        *failure_state.lock() = Some(ColdViewportFailure { requested: response_range.clone(), witness: failure_witness.clone() });
                        schedule_local_pane_main_thread(
                            promise::spawn::MainThreadServiceClass::Interactive,
                            LOCAL_PANE_MAIN_THREAD_ESTIMATED_BYTES,
                            "cold_viewport_failure",
                            || async move {
                                if completion.cancelled.load(Ordering::Acquire) { return; }
                                let _ = registration.try_with_current(|pane| {
                                    let Some(terminal) = terminal_for_failure.upgrade() else { return; };
                                    let Some(term) = terminal.try_lock() else { return; };
                                    let current = failure_witness.matches(term.screen());
                                    drop(term);
                                    if current {
                                        pane.dispatch_alert(Alert::ToastNotification {
                                            title: Some("Cold history unavailable".to_string()),
                                            body: if geometry { "This history needs a layout-index update before it can be displayed at this width. Stored content has not been changed." }
                                                else { "Cold history could not be loaded. Stored content has not been changed." }.to_string(),
                                            focus: false,
                                        });
                                    }
                                });
                            },
                        );
                    }
                    return;
                }
            };
            let Some(read) = plans.pop() else { return; };
            let (sender, retired) = sync_channel(1);
            let retirement = ColdViewportRetirement { read: Some(Arc::new(read)), evicted: Vec::with_capacity(1), sender };
            schedule_local_pane_main_thread(
                promise::spawn::MainThreadServiceClass::Interactive,
                LOCAL_PANE_MAIN_THREAD_ESTIMATED_BYTES,
                "cold_viewport_publish",
                || async move {
                    let mut retirement = retirement;
                    if completion.cancelled.load(Ordering::Acquire) {
                        retry_cold_viewport(registration, retry);
                        return;
                    }
                    if let Some(read) = retirement.read.as_ref().map(Arc::clone) {
                            let _ = registration.try_with_current(|pane| {
                                let Some(pending) = completion.state.try_lock() else {
                                    retry_cold_viewport(registration.clone(), Arc::clone(&retry));
                                    return;
                                };
                                if !pending.as_ref().is_some_and(|pending| Arc::ptr_eq(&pending.cancelled, &completion.cancelled)) { return; }
                                let mut published = false;
                                let _ = pane.publish_line_reads(std::slice::from_ref(read.as_ref()), &mut || {
                                    let Some(mut cache) = COLD_VIEWPORT_CACHE.try_lock() else { return; };
                                    if let Some(index) = cache.iter().position(|entry| entry.registration == registration.wire_identity()) {
                                        if let Some(entry) = cache.remove(index) { retirement.evicted.push(entry); }
                                    }
                                    while cache.len() >= 4 {
                                        if let Some(entry) = cache.pop_front() { retirement.evicted.push(entry); }
                                    }
                                    cache.push_back(ColdViewportEntry {
                                        registration: registration.wire_identity(), requested: response_range.clone(), read: Arc::clone(&read),
                                    });
                                    published = true;
                                });
                                drop(pending);
                                if published { pane.notify_lines_ready(); }
                                else { retry_cold_viewport(registration.clone(), Arc::clone(&retry)); }
                            });
                    }
                },
            );
            // Only the blocking worker waits. Queue cancellation drops the
            // retirement guard, so it also returns source ownership here.
            drop(retired.recv());
            drop(permit);
        });
        let worker = match worker {
            Ok(worker) => worker,
            Err(_) => {
                metrics::counter!("mux.local_pane.cold_viewport", "outcome" => "worker_rejected")
                    .increment(1);
                retry_cold_viewport(capture_registration, capture_retry);
                return empty();
            }
        };
        // Thread creation has succeeded before the first source clone. A
        // failed capture has no partial resident allocation (batch preflight),
        // and abandoning the handle retires the permit on its waiting worker.
        let Some(Ok(plan)) = self.capture_line_read(requested.clone(), &mut Default::default())
        else {
            drop(worker);
            retry_cold_viewport(capture_registration, capture_retry);
            return empty();
        };
        *captured_witness.lock() = Some(plan.failure_witness());
        worker.submit(vec![plan]);
        empty()
    }

    fn drain_scrollback_outside_terminal(
        &self,
        mut sink: Arc<dyn frankenterm_term::config::ScrollbackSpillSink>,
    ) {
        let mut reported_failure = false;
        let mut needs_flush = true;
        loop {
            if matches!(
                *self.process.lock(),
                ProcessState::Running { killed: true, .. }
                    | ProcessState::DeadPendingClose { killed: true }
                    | ProcessState::Dead
            ) {
                return;
            }
            let flushed = if needs_flush {
                sink.flush_scrollback()
            } else {
                Ok(())
            };
            let stalled = match flushed {
                Ok(()) => {
                    let mut terminal = self.locked_terminal();
                    let result = terminal.trim_deferred_scrollback();
                    let current = self.scrollback_flush_sink.lock().clone();
                    // Wake a queued reader before another geometry slice can
                    // reacquire the mutex. Durability still runs outside it.
                    MutexGuard::unlock_fair(terminal);
                    let Some(current) = current else {
                        return;
                    };
                    sink = current;
                    match result {
                        frankenterm_term::DeferredScrollbackTrim::Settled { moved: false } => {
                            if needs_flush {
                                return;
                            }
                            // Resize/configuration may settle the overflow
                            // between yielded slices. Earlier slices still
                            // own queued rows that must reach durability.
                            needs_flush = true;
                            false
                        }
                        frankenterm_term::DeferredScrollbackTrim::Yielded => {
                            needs_flush = false;
                            metrics::counter!("mux.scrollback.geometry_yields").increment(1);
                            // Opportunistic try-lock readers do not queue on
                            // the mutex; give their executor a scheduling turn.
                            std::thread::yield_now();
                            false
                        }
                        frankenterm_term::DeferredScrollbackTrim::Settled { moved }
                        | frankenterm_term::DeferredScrollbackTrim::AdmissionBlocked { moved } => {
                            // A yielded slice may have filled the pending
                            // queue. Flush that batch immediately: refusal
                            // before attempting durability is normal capacity
                            // backpressure, not a failed persistence attempt.
                            let stalled = needs_flush && !moved;
                            needs_flush = true;
                            stalled
                        }
                    }
                }
                Err(_) => true,
            };
            if stalled {
                if !reported_failure {
                    log::error!(
                        "pane {} scrollback persistence is stalled; retaining rows and applying parser backpressure outside the terminal lock",
                        self.pane_id
                    );
                    reported_failure = true;
                }
                metrics::counter!("mux.scrollback.persistence_backpressure").increment(1);
                // This is the blocking parser thread, not an async executor or
                // the GUI. Explicit pane close ends retries; failures never
                // permit another input batch to grow retained memory forever.
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }

    pub(crate) fn write_tmux_command_if_same(
        &self,
        expected: &TmuxDomainState,
        command: &str,
    ) -> Result<bool, Error> {
        // DCS parsing already holds the terminal lock before it installs or
        // clears a tmux binding. Preserve that lock order here so terminal
        // cleanup cannot deadlock parser-side terminal -> binding activity.
        // The blocking write itself runs on the tmux domain's supervised I/O
        // lane rather than the GUI/main lane.
        let mut terminal = self.locked_terminal();
        let tmux_domain = self.tmux_domain.lock();
        if !tmux_domain
            .as_ref()
            .is_some_and(|current| std::ptr::eq(current.as_ref(), expected))
        {
            return Ok(false);
        }

        // The terminal lock prevents DCS exit/re-entry from replacing the
        // validated binding before these bytes are submitted. Release the
        // binding mutex before the potentially blocking writer call so a
        // deadline supervisor can invalidate the binding and kill the
        // launcher without waiting on external I/O.
        drop(tmux_domain);
        terminal.send_paste(command)?;
        Ok(true)
    }

    pub(crate) fn clear_tmux_domain_if(&self, expected: &TmuxDomainState) -> bool {
        let mut tmux_domain = self.tmux_domain.lock();
        if tmux_domain
            .as_ref()
            .is_some_and(|current| std::ptr::eq(current.as_ref(), expected))
        {
            let _ = tmux_domain.take();
            true
        } else {
            false
        }
    }

    // ── ft-87qfi: lock-free SPSC disruptor staging for the pane->render path ──
    //
    // Every terminal access in this file goes through `locked_terminal()` rather
    // than `self.terminal.lock()` directly. With the `disruptor-pane-io` feature
    // OFF this is a zero-cost inline wrapper. With it ON, the single parser thread
    // (producer) may stage parsed action batches in `action_ring` instead of
    // blocking on the terminal mutex while the renderer reads; `locked_terminal`
    // drains the ring (FIFO) under the lock before returning the guard, so every
    // reader/writer observes all prior output applied in order. Correct-by-
    // construction: a single producer, and a consumer serialized by the terminal
    // mutex. Uses crossbeam `ArrayQueue` — a safe, vetted lock-free ring (no
    // `unsafe`).

    /// Lock the terminal, first draining any disruptor-staged action batches so
    /// the terminal reflects all parsed output before the caller observes it.
    #[cfg(feature = "disruptor-pane-io")]
    #[inline]
    fn locked_terminal(&self) -> MutexGuard<'_, Terminal> {
        let mut term = self.terminal.lock();
        self.drain_action_ring_locked(&mut term);
        term
    }

    /// Default build: a transparent wrapper around the terminal mutex.
    #[cfg(not(feature = "disruptor-pane-io"))]
    #[inline]
    fn locked_terminal(&self) -> MutexGuard<'_, Terminal> {
        self.terminal.lock()
    }

    /// Apply every staged action batch to `term`, in FIFO order, emptying the
    /// ring. Called only while the terminal mutex is held (so drains are
    /// serialized even though the producer pushes lock-free).
    #[cfg(feature = "disruptor-pane-io")]
    #[inline]
    fn drain_action_ring_locked(&self, term: &mut Terminal) {
        Self::drain_action_ring_into(self.action_ring.as_ref(), term);
    }

    /// Drain `action_ring` into `term` while the caller holds the terminal
    /// mutex. This is shared by normal `LocalPane` terminal access and the
    /// resize worker, whose static helper cannot call `locked_terminal`.
    #[cfg(feature = "disruptor-pane-io")]
    #[inline]
    fn drain_action_ring_into(action_ring: &ArrayQueue<Vec<Action>>, term: &mut Terminal) {
        while let Some(actions) = action_ring.pop() {
            term.perform_actions(actions);
        }
    }

    /// Producer side (parser thread). If the terminal lock is free, drain any
    /// staged batches and apply directly — identical to the mutex path, no
    /// deferral. If the renderer holds the lock, stage the batch in the lock-free
    /// ring and return immediately so the parser keeps parsing; the batch is
    /// applied (in order) by the next `locked_terminal` drain. If the ring is
    /// saturated, fall back to a blocking apply (back-pressure), draining first
    /// to preserve order.
    #[cfg(feature = "disruptor-pane-io")]
    fn perform_actions_disruptor(&self, actions: Vec<Action>) {
        if actions.is_empty() {
            return;
        }
        if let Some(mut term) = self.terminal.try_lock() {
            self.drain_action_ring_locked(&mut term);
            term.perform_actions(actions);
            return;
        }
        if let Err(actions) = self.action_ring.push(actions) {
            let mut term = self.terminal.lock();
            self.drain_action_ring_locked(&mut term);
            term.perform_actions(actions);
        }
    }

    /// Bench-only contention hook for the ft-87qfi harness. Holding the terminal
    /// lock while invoking `perform_actions` forces the feature-gated producer
    /// path to stage into the disruptor ring, so `mux/benches/event_bus.rs`
    /// measures the real contended pane-IO path rather than the uncontended
    /// direct-apply fast path.
    #[cfg(feature = "disruptor-pane-io")]
    #[doc(hidden)]
    pub fn bench_with_terminal_lock_held<F>(&self, f: F)
    where
        F: FnOnce(),
    {
        let _terminal = self.terminal.lock();
        f();
    }

    fn enqueue_resize(&self, size: TerminalSize, reconcile_tab: bool) -> Result<(), Error> {
        let pty_size = PtySize {
            rows: size.rows.try_into()?,
            cols: size.cols.try_into()?,
            pixel_width: size.pixel_width.try_into()?,
            pixel_height: size.pixel_height.try_into()?,
        };
        let enqueued_at = Instant::now();
        let completion_reservation = if reconcile_tab {
            match promise::spawn::try_reserve_main_thread(
                promise::spawn::MainThreadServiceClass::Topology,
                LOCAL_PANE_MAIN_THREAD_ESTIMATED_BYTES,
            ) {
                promise::spawn::MainThreadReservationOutcome::Reserved(reservation) => {
                    Some(reservation)
                }
                rejected => {
                    metrics::counter!(
                        "mux.localpane.resize.intent_rejected",
                        "reason" => "completion_capacity",
                    )
                    .increment(1);
                    anyhow::bail!("remote resize completion admission rejected: {rejected:?}");
                }
            }
        } else {
            None
        };

        let enqueue_result = {
            let mut queue = self.resize_queue.lock();
            queue.try_enqueue(
                size,
                pty_size,
                enqueued_at,
                reconcile_tab,
                completion_reservation,
            )
        };
        let outcome = match enqueue_result {
            Ok(outcome) => outcome,
            Err(ResizeEnqueueError::SequenceExhausted) => {
                metrics::counter!(
                    "mux.localpane.resize.intent_rejected",
                    "reason" => "sequence_exhausted",
                )
                .increment(1);
                anyhow::bail!(
                    "resize generation exhausted for pane_id={}; refusing ambiguous resize intent",
                    self.pane_id
                );
            }
        };

        log::trace!(
            "LocalPane::resize enqueue pane_id={} seq={} target={}x{} replaced_seq={:?} queue_depth_hint={} worker_spawned={}",
            self.pane_id,
            outcome.seq,
            size.cols,
            size.rows,
            outcome.replaced_seq,
            outcome.queue_depth_hint,
            outcome.spawn_worker
        );

        if outcome.spawn_worker {
            Self::spawn_resize_worker(
                self.pane_id,
                Arc::clone(&self.terminal),
                Arc::clone(&self.line_layout_observation),
                #[cfg(feature = "disruptor-pane-io")]
                Arc::clone(&self.action_ring),
                Arc::clone(&self.pty),
                Arc::clone(&self.resize_queue),
                Arc::clone(&self.mux_registration),
            );
        }

        Ok(())
    }

    fn spawn_resize_worker(
        pane_id: PaneId,
        terminal: Arc<Mutex<Terminal>>,
        line_layout_observation: Arc<LineLayoutObservation>,
        #[cfg(feature = "disruptor-pane-io")] action_ring: Arc<ArrayQueue<Vec<Action>>>,
        pty: Arc<Mutex<Box<dyn MasterPty>>>,
        resize_queue: Arc<Mutex<ResizeQueueState>>,
        registration: Arc<PaneRegistrationSlot>,
    ) {
        let worker_terminal = Arc::clone(&terminal);
        let worker_line_layout_observation = Arc::clone(&line_layout_observation);
        #[cfg(feature = "disruptor-pane-io")]
        let worker_action_ring = Arc::clone(&action_ring);
        let worker_pty = Arc::clone(&pty);
        let worker_queue = Arc::clone(&resize_queue);
        let worker_registration = Arc::clone(&registration);
        let spawn_result = std::thread::Builder::new()
            .name(format!("pane-resize-{}", pane_id))
            .spawn(move || {
                Self::run_resize_worker(
                    pane_id,
                    worker_terminal,
                    worker_line_layout_observation,
                    #[cfg(feature = "disruptor-pane-io")]
                    worker_action_ring,
                    worker_pty,
                    worker_queue,
                    worker_registration,
                    true,
                );
            });

        if let Err(err) = &spawn_result {
            log::error!(
                "failed to spawn resize worker; settling inline pane_id={} error={:#}",
                pane_id,
                err
            );
        }
        settle_resize_worker_spawn(spawn_result, || {
            // The queue still owns the latest coalesced target and still marks
            // this worker as running. Drain it on the caller rather than
            // clearing admission and stranding the final resize indefinitely.
            // Thread creation failure is exceptional; correctness takes
            // precedence over keeping this rare fallback off the caller.
            Self::run_resize_worker(
                pane_id,
                terminal,
                line_layout_observation,
                #[cfg(feature = "disruptor-pane-io")]
                action_ring,
                pty,
                resize_queue,
                registration,
                false,
            );
        });
    }

    fn run_resize_worker(
        pane_id: PaneId,
        terminal: Arc<Mutex<Terminal>>,
        line_layout_observation: Arc<LineLayoutObservation>,
        #[cfg(feature = "disruptor-pane-io")] action_ring: Arc<ArrayQueue<Vec<Action>>>,
        pty: Arc<Mutex<Box<dyn MasterPty>>>,
        resize_queue: Arc<Mutex<ResizeQueueState>>,
        registration: Arc<PaneRegistrationSlot>,
        allow_cold_preparation: bool,
    ) {
        while let Some(pending) = {
            let mut queue = resize_queue.lock();
            queue.dequeue_for_worker()
        } {
            let queue_wait = pending.enqueued_at.elapsed();
            let completion_start = Instant::now();
            let token = ResizeCancellationToken::new(pending.seq);
            let pending_registration = registration.load();
            let apply_result = catch_resize_intent(resize_queue.as_ref(), pending, || {
                Self::apply_resize_sync(
                    pane_id,
                    terminal.as_ref(),
                    line_layout_observation.as_ref(),
                    #[cfg(feature = "disruptor-pane-io")]
                    action_ring.as_ref(),
                    pty.as_ref(),
                    resize_queue.as_ref(),
                    pending.seq,
                    pending.size,
                    pending.pty_size,
                    token,
                )
            });
            let settled_apply_result = apply_result
                .map(|result| recover_resize_apply_error(resize_queue.as_ref(), pending, result));
            if matches!(&settled_apply_result, Ok(Ok(metrics)) if !metrics.cancelled) {
                if let Some(registration) = pending_registration {
                    // Reconcile primary committed size before cold preparation waits,
                    // covering required primary completion with its pre-admitted permit.
                    Self::schedule_resize_completion(&resize_queue, token, registration.clone());
                    if allow_cold_preparation {
                        Self::prepare_cold_layout_after_resize(
                            pane_id,
                            &terminal,
                            &line_layout_observation,
                            &resize_queue,
                            token,
                            registration,
                        );
                    }
                }
            }
            match settled_apply_result {
                Ok(Ok(metrics)) => {
                    if metrics.cancelled {
                        log::trace!(
                            "LocalPane::resize cancelled pane_id={} seq={} commit_id={} rejected_frame={} superseded_by_seq={} stage={} queue_wait_us={} completion_us={} current={}x{} target={}x{} probe_lock_wait_us={} pty_lock_wait_us={} pty_resize_us={} pty_resize_attempts={} pty_retry_backoff_us={} swap_barrier_wait_us={} terminal_apply_lock_wait_us={} terminal_resize_us={}",
                            pane_id,
                            pending.seq,
                            metrics.commit_id,
                            metrics.rejected_frame,
                            metrics.superseded_by_seq.unwrap_or_default(),
                            metrics.cancelled_stage.unwrap_or("unknown"),
                            queue_wait.as_micros(),
                            completion_start.elapsed().as_micros(),
                            metrics.current_size.cols,
                            metrics.current_size.rows,
                            metrics.target_size.cols,
                            metrics.target_size.rows,
                            metrics.probe_lock_wait.as_micros(),
                            metrics.pty_lock_wait.as_micros(),
                            metrics.pty_resize_elapsed.as_micros(),
                            metrics.pty_resize_attempts,
                            metrics.pty_retry_backoff_elapsed.as_micros(),
                            metrics.swap_barrier_wait.as_micros(),
                            metrics.terminal_apply_lock_wait.as_micros(),
                            metrics.terminal_resize_elapsed.as_micros(),
                        );
                    } else {
                        log::trace!(
                            "LocalPane::resize complete pane_id={} seq={} commit_id={} rejected_frame={} queue_wait_us={} completion_us={} noop={} current={}x{} target={}x{} probe_lock_wait_us={} pty_lock_wait_us={} pty_resize_us={} pty_resize_attempts={} pty_retry_backoff_us={} swap_barrier_wait_us={} terminal_apply_lock_wait_us={} terminal_resize_us={}",
                            pane_id,
                            pending.seq,
                            metrics.commit_id,
                            metrics.rejected_frame,
                            queue_wait.as_micros(),
                            completion_start.elapsed().as_micros(),
                            metrics.noop,
                            metrics.current_size.cols,
                            metrics.current_size.rows,
                            metrics.target_size.cols,
                            metrics.target_size.rows,
                            metrics.probe_lock_wait.as_micros(),
                            metrics.pty_lock_wait.as_micros(),
                            metrics.pty_resize_elapsed.as_micros(),
                            metrics.pty_resize_attempts,
                            metrics.pty_retry_backoff_elapsed.as_micros(),
                            metrics.swap_barrier_wait.as_micros(),
                            metrics.terminal_apply_lock_wait.as_micros(),
                            metrics.terminal_resize_elapsed.as_micros(),
                        );
                    }
                }
                Ok(Err((err, recovery))) => {
                    record_resize_failure(ResizeFailureKind::ApplyError, recovery);
                    match recovery {
                        ResizeFailureRecovery::Requeued { retry } => {
                            log::error!(
                                "LocalPane::resize apply error pane_id={} seq={} target={}x{} retry={}/{} action=requeued error={:#}",
                                pane_id,
                                pending.seq,
                                pending.size.cols,
                                pending.size.rows,
                                retry,
                                MAX_RESIZE_APPLY_ERROR_RETRIES,
                                err,
                            );
                        }
                        ResizeFailureRecovery::Superseded { by_seq } => {
                            log::error!(
                                "LocalPane::resize apply error pane_id={} seq={} target={}x{} action=superseded superseded_by_seq={} error={:#}",
                                pane_id,
                                pending.seq,
                                pending.size.cols,
                                pending.size.rows,
                                by_seq,
                                err,
                            );
                        }
                        ResizeFailureRecovery::ExhaustedRetained { retries } => {
                            log::error!(
                                "LocalPane::resize apply error retry budget exhausted pane_id={} seq={} target={}x{} retries={} action=retained_worker_released error={:#}",
                                pane_id,
                                pending.seq,
                                pending.size.cols,
                                pending.size.rows,
                                retries,
                                err,
                            );
                            return;
                        }
                    }
                }
                Err(recovery) => {
                    record_resize_failure(ResizeFailureKind::RecoverablePanic, recovery);
                    match recovery {
                        ResizeFailureRecovery::Requeued { retry } => {
                            log::error!(
                                "LocalPane::resize recovered callback panic pane_id={} seq={} target={}x{} retry={}/{} action=requeued",
                                pane_id,
                                pending.seq,
                                pending.size.cols,
                                pending.size.rows,
                                retry,
                                MAX_RESIZE_RECOVERABLE_PANIC_RETRIES,
                            );
                        }
                        ResizeFailureRecovery::Superseded { by_seq } => {
                            log::error!(
                                "LocalPane::resize recovered callback panic pane_id={} seq={} target={}x{} action=superseded superseded_by_seq={}",
                                pane_id,
                                pending.seq,
                                pending.size.cols,
                                pending.size.rows,
                                by_seq,
                            );
                        }
                        ResizeFailureRecovery::ExhaustedRetained { retries } => {
                            log::error!(
                                "LocalPane::resize callback panic retry budget exhausted pane_id={} seq={} target={}x{} retries={} action=retained_worker_released",
                                pane_id,
                                pending.seq,
                                pending.size.cols,
                                pending.size.rows,
                                retries,
                            );
                            return;
                        }
                    }
                }
            }
        }
    }

    /// Runs only on a successfully spawned resize worker, never the inline
    /// thread-creation-failure fallback. This worker retains the newest intent
    /// across contention: abandoning a seam leaves later all-history reads
    /// unable to cross its old-width cold/resident boundary.
    fn prepare_cold_layout_after_resize(
        pane_id: PaneId,
        terminal: &Mutex<Terminal>,
        line_layout_observation: &LineLayoutObservation,
        resize_queue: &Arc<Mutex<ResizeQueueState>>,
        token: ResizeCancellationToken,
        registration: PaneRegistrationHandle,
    ) {
        let cancelled = || {
            resize_queue.lock().superseded_by(token).is_some()
                || registration.try_with_current(|_| ()).is_none()
        };
        let result = catch_recoverable(
            RecoverablePanicSite::MuxPaneCallback,
            AssertUnwindSafe(|| -> anyhow::Result<bool> {
                // No payload or other admission is held while waiting for the
                // shared four-worker limit. Once admitted, this same permit
                // covers seam hydration, retained retries and index work.
                let Some(_permit) = retry_cold_resize_step("admission", &cancelled, || {
                    Ok(crate::pane::LineReadPermit::try_acquire())
                })?
                else {
                    return Ok(false);
                };
                let settled = retry_cold_resize_step("seam_recapture", &cancelled, || {
                    let Some(seam) = retry_cold_resize_step("seam_capture", &cancelled, || {
                        registration
                            .try_with_current(|_| {
                                let Some(term) = terminal.try_lock() else {
                                    return Ok(None);
                                };
                                term.screen().capture_cold_seam_reflow().map(Some)
                            })
                            .unwrap_or(Ok(None))
                    })?
                    else {
                        return Ok(None);
                    };
                    let Some(seam) = seam else {
                        return Ok(Some(()));
                    };
                    let mut seam = seam.hydrate(&cancelled)?;
                    if !seam.is_ready() {
                        return Ok(Some(()));
                    }
                    let installed = retry_cold_resize_step("seam_commit", &cancelled, || {
                        registration
                            .try_with_current(|_| {
                                let Some(mut term) = terminal.try_lock() else {
                                    return Ok(None);
                                };
                                let (decision, _) =
                                    with_resize_commit_barrier(resize_queue, token, || {
                                        let seqno =
                                            next_cold_resize_sequence(term.current_seqno())?;
                                        let installed = term
                                            .screen_mut()
                                            .install_cold_seam_reflow(&mut seam, seqno)?;
                                        if installed {
                                            term.increment_seqno();
                                            Self::publish_resize_source(
                                                pane_id,
                                                line_layout_observation,
                                                &mut term,
                                                token,
                                                token.seq,
                                                "cold_seam",
                                            );
                                        }
                                        Ok::<_, anyhow::Error>(installed)
                                    });
                                match decision {
                                    ResizeCommitDecision::Committed(result) => result.map(Some),
                                    ResizeCommitDecision::Superseded { .. } => Ok(None),
                                }
                            })
                            .unwrap_or(Ok(None))
                    })?;
                    if installed == Some(true) {
                        Ok(Some(()))
                    } else {
                        // The token may still be newest while output/pruning
                        // changes its source. Retire this stale payload here,
                        // then recapture exact current authority. Unavailable
                        // metadata or decode errors remain finite failures.
                        Ok(None)
                    }
                })?;
                if settled.is_none() {
                    return Ok(false);
                }
                let indexed = retry_cold_resize_step("index_recapture", &cancelled, || {
                    let Some(plan) = retry_cold_resize_step("index_capture", &cancelled, || {
                        registration
                            .try_with_current(|_| -> anyhow::Result<_> {
                                let Some(term) = terminal.try_lock() else {
                                    return Ok(None);
                                };
                                let screen = term.screen();
                                // Capture clamps this one-row request to the
                                // oldest reachable visual row using one coherent
                                // interval. A separate top-row probe can mistake
                                // Busy metadata for an empty cold tier.
                                let plan = screen.capture_line_read(
                                    StableRowIndex::MIN..StableRowIndex::MIN + 1,
                                )?;
                                if plan.first_row() >= screen.phys_to_stable_row_index(0) {
                                    return Ok(Some(None));
                                }
                                Ok(Some(Some(plan)))
                            })
                            .unwrap_or(Ok(None))
                    })?
                    else {
                        return Ok(None);
                    };
                    let Some(plan) = plan else {
                        return Ok(Some(false));
                    };
                    let mut prepared = plan.prepare_cold_layout(&cancelled)?;
                    let installed = retry_cold_resize_step("index_commit", &cancelled, || {
                        registration
                            .try_with_current(|_| {
                                let Some(mut term) = terminal.try_lock() else {
                                    return Ok(None);
                                };
                                let (decision, _) =
                                    with_resize_commit_barrier(resize_queue, token, || {
                                        if term.current_seqno() == SequenceNo::MAX {
                                            anyhow::bail!("cold index sequence exhausted");
                                        }
                                        if !term
                                            .screen()
                                            .validates_prepared_cold_layout(&prepared)?
                                        {
                                            return Ok(false);
                                        }
                                        let changes_layout = term
                                            .screen()
                                            .prepared_cold_layout_changes_layout(&prepared);
                                        let seqno = if changes_layout {
                                            next_cold_resize_sequence(term.current_seqno())?
                                        } else {
                                            term.current_seqno()
                                        };
                                        if !term
                                            .screen_mut()
                                            .install_prepared_cold_layout(&mut prepared, seqno)?
                                        {
                                            return Ok(false);
                                        }
                                        if changes_layout {
                                            term.increment_seqno();
                                        }
                                        Self::publish_resize_source(
                                            pane_id,
                                            line_layout_observation,
                                            &mut term,
                                            token,
                                            token.seq,
                                            "cold_index",
                                        );
                                        Ok::<_, anyhow::Error>(true)
                                    });
                                match decision {
                                    ResizeCommitDecision::Committed(result) => result.map(Some),
                                    ResizeCommitDecision::Superseded { .. } => Ok(None),
                                }
                            })
                            .unwrap_or(Ok(None))
                    })?;
                    if installed == Some(true) {
                        Ok(Some(true))
                    } else {
                        // The token may still be newest while output/pruning
                        // changes its source or frontier. Retire this stale
                        // payload here, then recapture exact current authority.
                        // Unavailable metadata or decode errors remain finite failures.
                        drop(prepared);
                        Ok(None)
                    }
                })?;
                let committed = indexed.unwrap_or(false);
                // The preparation and displaced layout retire here on this
                // worker, after both locks and before the global permit.
                Ok(committed)
            }),
        );
        // The primary terminal resize has already committed. A missing cold
        // tier or unavailable history must not suppress its visible-frame
        // wakeup and leave the GUI waiting for the repaint retry timer.
        // This is only a hint: frame capture still validates the exact source.
        Self::schedule_resize_completion(resize_queue, token, registration);
        metrics::counter!("mux.localpane.resize.cold_layout", "outcome" => match result {
            Ok(Ok(true)) => "installed",
            Ok(Ok(false)) => "not_installed",
            Ok(Err(_)) => "unavailable",
            Err(_) => "recovered_panic",
        })
        .increment(1);
        if !matches!(result, Ok(Ok(true))) {
            log::debug!(
                "LocalPane::resize cold_layout pane_id={pane_id} seq={} outcome={}",
                token.seq,
                match result {
                    Ok(Ok(false)) => "not_installed",
                    Ok(Err(_)) => "source_or_geometry_unavailable",
                    Err(_) => "recovered_panic",
                    Ok(Ok(true)) => "installed",
                }
            );
        }
    }

    fn schedule_resize_completion(
        resize_queue: &Arc<Mutex<ResizeQueueState>>,
        token: ResizeCancellationToken,
        registration: PaneRegistrationHandle,
    ) {
        let (reservation, reconcile_tab) = {
            let mut queue = resize_queue.lock();
            if queue.superseded_by(token).is_some() {
                return;
            }
            let reservation = queue.completion_reservation.take();
            let reconcile_tab = std::mem::replace(&mut queue.reconcile_tab_on_completion, false);
            (reservation, reconcile_tab)
        };
        // Taking before this check also releases reserved capacity when the
        // pane retired during preparation. A newer intent retains its permit.
        if registration.try_with_current(|_| ()).is_none() {
            return;
        }
        let resize_queue = Arc::clone(resize_queue);
        let make_future = move || async move {
            // Admission is not commit authority: a newer request can arrive
            // after this callback was queued. Never notify for that old intent.
            let superseded = {
                let queue = resize_queue.lock();
                queue.superseded_by(token).is_some()
            };
            if superseded {
                return;
            }
            let _ = registration.try_with_current(|pane| {
                if reconcile_tab {
                    pane.notify_resize_completed();
                } else {
                    pane.notify_lines_ready();
                }
            });
        };
        if let Some(reservation) = reservation {
            reservation.spawn(make_future()).detach();
        } else {
            schedule_local_pane_main_thread(
                promise::spawn::MainThreadServiceClass::Interactive,
                LOCAL_PANE_MAIN_THREAD_ESTIMATED_BYTES,
                "resize_layout_settled",
                make_future,
            );
        }
    }

    fn prepare_resize_reflow(
        terminal: &Mutex<Terminal>,
        #[cfg(feature = "disruptor-pane-io")] action_ring: &ArrayQueue<Vec<Action>>,
        size: TerminalSize,
        is_cancelled: impl Fn() -> bool,
    ) -> (Option<frankenterm_term::ScreenReflowPreparation>, Duration) {
        let capture_start = Instant::now();
        let mut prepared = {
            let terminal = terminal.lock();
            #[cfg(feature = "disruptor-pane-io")]
            let mut terminal = terminal;
            #[cfg(feature = "disruptor-pane-io")]
            Self::drain_action_ring_into(action_ring, &mut terminal);
            terminal.capture_reflow_preparation(size)
        };
        let capture_elapsed = capture_start.elapsed();
        if let Some(work) = prepared.as_mut() {
            if !work.prepare(is_cancelled) {
                prepared = None;
            }
        }
        (prepared, capture_elapsed)
    }

    fn apply_resize_sync(
        pane_id: PaneId,
        terminal: &Mutex<Terminal>,
        line_layout_observation: &LineLayoutObservation,
        #[cfg(feature = "disruptor-pane-io")] action_ring: &ArrayQueue<Vec<Action>>,
        pty: &Mutex<Box<dyn MasterPty>>,
        resize_queue: &Mutex<ResizeQueueState>,
        commit_id: u64,
        size: TerminalSize,
        pty_size: PtySize,
        token: ResizeCancellationToken,
    ) -> Result<ResizeApplyMetrics, Error> {
        let terminal_probe_lock_start = Instant::now();
        #[cfg(feature = "disruptor-pane-io")]
        let current_size = {
            let mut terminal = terminal.lock();
            Self::drain_action_ring_into(action_ring, &mut terminal);
            terminal.get_size()
        };
        #[cfg(not(feature = "disruptor-pane-io"))]
        let current_size = terminal.lock().get_size();
        let terminal_probe_lock_wait = terminal_probe_lock_start.elapsed();

        let (superseded_by_seq, last_proven_pty_size) = {
            let queue = resize_queue.lock();
            (queue.superseded_by(token), queue.last_proven_pty_size)
        };
        if let Some(superseded_by_seq) = superseded_by_seq {
            return Ok(ResizeApplyMetrics {
                commit_id,
                current_size,
                target_size: size,
                probe_lock_wait: terminal_probe_lock_wait,
                pty_lock_wait: Duration::default(),
                pty_resize_elapsed: Duration::default(),
                pty_resize_attempts: 0,
                pty_retry_backoff_elapsed: Duration::default(),
                swap_barrier_wait: Duration::default(),
                terminal_apply_lock_wait: Duration::default(),
                terminal_resize_elapsed: Duration::default(),
                noop: false,
                rejected_frame: true,
                cancelled: true,
                cancelled_stage: Some("before_pty_resize"),
                superseded_by_seq: Some(superseded_by_seq),
            });
        }

        if resize_is_proven_noop(current_size, size, last_proven_pty_size, pty_size) {
            return Ok(ResizeApplyMetrics {
                commit_id,
                current_size,
                target_size: size,
                probe_lock_wait: terminal_probe_lock_wait,
                pty_lock_wait: Duration::default(),
                pty_resize_elapsed: Duration::default(),
                pty_resize_attempts: 0,
                pty_retry_backoff_elapsed: Duration::default(),
                swap_barrier_wait: Duration::default(),
                terminal_apply_lock_wait: Duration::default(),
                terminal_resize_elapsed: Duration::default(),
                noop: true,
                rejected_frame: false,
                cancelled: false,
                cancelled_stage: None,
                superseded_by_seq: None,
            });
        }

        let pty_size_is_proven = last_proven_pty_size == Some(pty_size);
        let mut pty_lock_wait = Duration::default();
        let mut pty_resize_elapsed = Duration::default();
        let retry_stats = if pty_size_is_proven {
            ResizeRetryStats::default()
        } else {
            // A failed or panicking PTY callback can leave the kernel-side
            // geometry ambiguous. Invalidate proof before the first attempt;
            // only a completed callback below may restore it.
            resize_queue.lock().last_proven_pty_size = None;
            let policy = pty_resize_retry_policy();
            let retry_result = retry_with_backoff_controlled(policy, |attempt| {
                if let Some(by_seq) = resize_queue.lock().superseded_by(token) {
                    return Err(RetryStepError::Stop(PtyResizeAttemptFailure::Superseded {
                        by_seq,
                    }));
                }
                let pty_lock_start = Instant::now();
                let pty = pty.lock();
                pty_lock_wait += pty_lock_start.elapsed();
                if let Some(by_seq) = resize_queue.lock().superseded_by(token) {
                    drop(pty);
                    return Err(RetryStepError::Stop(PtyResizeAttemptFailure::Superseded {
                        by_seq,
                    }));
                }
                let pty_resize_start = Instant::now();
                let result = pty.resize(pty_size);
                pty_resize_elapsed += pty_resize_start.elapsed();
                drop(pty);
                if let Err(err) = result {
                    log::warn!(
                        "LocalPane::resize pty retry pane_id={} attempt={}/{} target={}x{} error={:#}",
                        pane_id,
                        attempt,
                        policy.max_attempts,
                        size.cols,
                        size.rows,
                        err
                    );
                    return Err(RetryStepError::Retry(PtyResizeAttemptFailure::Apply(err)));
                }
                Ok(())
            });
            let retry_stats = match retry_result {
                Ok(((), stats)) => stats,
                Err((PtyResizeAttemptFailure::Superseded { by_seq }, stats)) => {
                    return Ok(ResizeApplyMetrics {
                        commit_id,
                        current_size,
                        target_size: size,
                        probe_lock_wait: terminal_probe_lock_wait,
                        pty_lock_wait,
                        pty_resize_elapsed,
                        pty_resize_attempts: stats.attempts.saturating_sub(1),
                        pty_retry_backoff_elapsed: stats.backoff_elapsed,
                        swap_barrier_wait: Duration::default(),
                        terminal_apply_lock_wait: Duration::default(),
                        terminal_resize_elapsed: Duration::default(),
                        noop: false,
                        rejected_frame: true,
                        cancelled: true,
                        cancelled_stage: Some("before_pty_retry"),
                        superseded_by_seq: Some(by_seq),
                    });
                }
                Err((PtyResizeAttemptFailure::Apply(err), stats)) => {
                    return Err(err.context(format!(
                        "pty resize failed after {} attempts for pane_id={} target={}x{}",
                        stats.attempts, pane_id, size.cols, size.rows
                    )));
                }
            };
            resize_queue.lock().last_proven_pty_size = Some(pty_size);
            retry_stats
        };

        if let Some(superseded_by_seq) = resize_queue.lock().superseded_by(token) {
            return Ok(ResizeApplyMetrics {
                commit_id,
                current_size,
                target_size: size,
                probe_lock_wait: terminal_probe_lock_wait,
                pty_lock_wait,
                pty_resize_elapsed,
                pty_resize_attempts: retry_stats.attempts,
                pty_retry_backoff_elapsed: retry_stats.backoff_elapsed,
                swap_barrier_wait: Duration::default(),
                terminal_apply_lock_wait: Duration::default(),
                terminal_resize_elapsed: Duration::default(),
                noop: false,
                rejected_frame: true,
                cancelled: true,
                cancelled_stage: Some("before_terminal_apply"),
                superseded_by_seq: Some(superseded_by_seq),
            });
        }

        // Capture COW lines and a shared logical cache, then perform the costly
        // wrap planning/materialization without either admission or terminal
        // locks. The live resize below validates the exact source before reuse.
        // One existing pane worker owns this work; newer intents cancel it at
        // bounded batch boundaries and still pass the final commit barrier.
        let reflow_prepare_start = Instant::now();
        static DISABLE_PREPARATION: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let (mut prepared_reflow, reflow_capture_elapsed) =
            if *DISABLE_PREPARATION.get_or_init(|| {
                std::env::var_os("FT_DISABLE_PREPARED_REFLOW").is_some_and(|value| value == "1")
            }) {
                (None, Duration::ZERO)
            } else {
                Self::prepare_resize_reflow(
                    terminal,
                    #[cfg(feature = "disruptor-pane-io")]
                    action_ring,
                    size,
                    || resize_queue.lock().superseded_by(token).is_some(),
                )
            };
        log::trace!(
            "LocalPane::resize prepare pane_id={} seq={} capture_us={} prepare_us={} ready={}",
            pane_id,
            token.seq,
            reflow_capture_elapsed.as_micros(),
            reflow_prepare_start
                .elapsed()
                .saturating_sub(reflow_capture_elapsed)
                .as_micros(),
            prepared_reflow.is_some(),
        );

        let terminal_apply_lock_start = Instant::now();
        let mut terminal = terminal.lock();
        #[cfg(feature = "disruptor-pane-io")]
        Self::drain_action_ring_into(action_ring, &mut terminal);
        let terminal_apply_lock_wait = terminal_apply_lock_start.elapsed();
        let (commit_decision, swap_barrier_wait) =
            with_resize_commit_barrier(resize_queue, token, || {
                if terminal.get_size() == size {
                    return Duration::default();
                }
                let terminal_resize_start = Instant::now();
                terminal.resize_with_prepared_reflow(size, prepared_reflow.as_mut());
                let terminal_resize_elapsed = terminal_resize_start.elapsed();
                // Publish the committed source while both guards still exclude
                // a newer intent and GUI frame capture. Worker completion below
                // includes off-lock retirement/cold preparation and can follow
                // presentation; it is not the frame's causal commit boundary.
                Self::publish_resize_source(
                    pane_id,
                    line_layout_observation,
                    &mut terminal,
                    token,
                    commit_id,
                    "primary",
                );
                terminal_resize_elapsed
            });
        drop(terminal);
        if let Some(prepared) = prepared_reflow.as_ref() {
            metrics::counter!(
                "mux.localpane.resize.prepared_reflow",
                "outcome" => if prepared.was_applied() { "applied" } else { "not_applied" },
            )
            .increment(1);
            log::trace!(
                "LocalPane::resize prepared_commit pane_id={} seq={} applied={}",
                pane_id,
                token.seq,
                prepared.was_applied(),
            );
        }
        // Cache replacement and cancelled snapshots can retire large histories.
        // Their last references must not be destroyed under either UI lock.
        drop(prepared_reflow);
        let terminal_resize_elapsed = match commit_decision {
            ResizeCommitDecision::Committed(elapsed) => elapsed,
            ResizeCommitDecision::Superseded {
                by_seq: superseded_by_seq,
            } => {
                return Ok(ResizeApplyMetrics {
                    commit_id,
                    current_size,
                    target_size: size,
                    probe_lock_wait: terminal_probe_lock_wait,
                    pty_lock_wait,
                    pty_resize_elapsed,
                    pty_resize_attempts: retry_stats.attempts,
                    pty_retry_backoff_elapsed: retry_stats.backoff_elapsed,
                    swap_barrier_wait,
                    terminal_apply_lock_wait,
                    terminal_resize_elapsed: Duration::default(),
                    noop: false,
                    rejected_frame: true,
                    cancelled: true,
                    cancelled_stage: Some("before_present_commit"),
                    superseded_by_seq: Some(superseded_by_seq),
                });
            }
        };

        Ok(ResizeApplyMetrics {
            commit_id,
            current_size,
            target_size: size,
            probe_lock_wait: terminal_probe_lock_wait,
            pty_lock_wait,
            pty_resize_elapsed,
            pty_resize_attempts: retry_stats.attempts,
            pty_retry_backoff_elapsed: retry_stats.backoff_elapsed,
            swap_barrier_wait,
            terminal_apply_lock_wait,
            terminal_resize_elapsed,
            noop: false,
            rejected_frame: false,
            cancelled: false,
            cancelled_stage: None,
            superseded_by_seq: None,
        })
    }

    pub fn new(
        pane_id: PaneId,
        terminal: Terminal,
        process: Box<dyn Child + Send>,
        pty: Box<dyn MasterPty>,
        writer: Box<dyn Write + Send>,
        domain_id: DomainId,
        durable_pane_id: [u8; 16],
        command_description: String,
    ) -> Self {
        Self::new_with_ownership(
            pane_id,
            terminal,
            process,
            pty,
            writer,
            domain_id,
            durable_pane_id,
            command_description,
            None,
            None,
            LocalPaneOwnership::LegacyMuxOwned,
        )
    }

    /// Construct a LocalPane over guardian-backed PTY/process proxy objects.
    ///
    /// Write, resize, status, and signal operations continue through the
    /// existing object-safe portable-pty interfaces supplied by the caller.
    /// Output uses the record-aware reader so authenticated guardian receipts
    /// remain attached to their exact plaintext through parser delivery.
    /// The explicit ownership value changes only lifetime behavior: `kill`
    /// performs one fenced guardian close, while dropping the mux-side pane
    /// retires only its lease and never invokes the child killer. This
    /// constructor consumes, but does not itself create or authenticate, those
    /// transport and replay authorities.
    #[allow(clippy::too_many_arguments)]
    pub fn new_guardian_proxy(
        pane_id: PaneId,
        terminal: Terminal,
        process: Box<dyn Child + Send>,
        pty: Box<dyn MasterPty>,
        writer: Box<dyn Write + Send>,
        domain_id: DomainId,
        lease_identity: GuardianPaneLeaseIdentity,
        lease_control: Arc<dyn GuardianPaneLeaseControl>,
        command_description: String,
        guardian_live_output_reader: Box<dyn GuardianLiveOutputReader>,
        guardian_checkpoint_publisher: Arc<dyn GuardianLiveCheckpointPublisher>,
        spawn_custody: Option<crate::guardian_checkpoint::GuardianSpawnCustodyScopeV1>,
        restored_prefix: Option<crate::guardian_checkpoint::GuardianRestoredParserPrefix>,
    ) -> Self {
        let mut pane = Self::new_with_ownership(
            pane_id,
            terminal,
            process,
            pty,
            writer,
            domain_id,
            *lease_identity.pane_id().as_bytes(),
            command_description,
            Some(guardian_live_output_reader),
            Some(guardian_checkpoint_publisher),
            LocalPaneOwnership::guardian(lease_identity, lease_control, spawn_custody),
        );
        *pane.guardian_restored_prefix.get_mut() = restored_prefix;
        pane
    }

    /// Check mux-owned publication prerequisites without performing I/O or
    /// minting any transport, parser, or lease authority.
    pub(crate) fn validate_unpublished_guardian_proxy(&self) -> anyhow::Result<()> {
        let LocalPaneOwnership::Guardian(ownership) = &self.ownership else {
            anyhow::bail!("unpublished guardian pane requires guardian ownership");
        };
        if *ownership.disposition.lock() != GuardianLeaseDisposition::Attached
            || self.durable_pane_id != *ownership.identity.pane_id().as_bytes()
            || self.mux_registration.load().is_some()
            || self.guardian_live_output_reader.lock().is_none()
            || self.guardian_checkpoint_publisher.is_none()
        {
            anyhow::bail!("unpublished guardian pane has incomplete or consumed authority");
        }
        if let Some(scope) = ownership.spawn_custody {
            if scope.pane_id != ownership.identity.pane_id()
                || scope.guardian_incarnation != ownership.identity.guardian_incarnation()
                || (ownership.identity.generation() == 1
                    && scope.mux_incarnation != ownership.identity.mux_incarnation())
            {
                anyhow::bail!("guardian birth provenance does not match live lease");
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn new_with_ownership(
        pane_id: PaneId,
        mut terminal: Terminal,
        process: Box<dyn Child + Send>,
        pty: Box<dyn MasterPty>,
        writer: Box<dyn Write + Send>,
        domain_id: DomainId,
        durable_pane_id: [u8; 16],
        command_description: String,
        guardian_live_output_reader: Option<Box<dyn GuardianLiveOutputReader>>,
        guardian_checkpoint_publisher: Option<Arc<dyn GuardianLiveCheckpointPublisher>>,
        ownership: LocalPaneOwnership,
    ) -> Self {
        let mux_registration = Arc::new(PaneRegistrationSlot::default());
        let child_exit_prune = ChildExitPruneState::new(Arc::clone(&mux_registration));
        let tmux_domain = Arc::new(Mutex::new(None));
        let (process, signaller, pid) = split_child(process, Arc::clone(&child_exit_prune));

        terminal.set_device_control_handler(Box::new(LocalPaneDCSHandler {
            pane_id,
            tmux_domain: Arc::clone(&tmux_domain),
            mux_registration: Arc::clone(&mux_registration),
        }));
        terminal.set_notification_handler(Box::new(LocalPaneNotifHandler {
            pane_id,
            mux_registration: Arc::clone(&mux_registration),
        }));

        let process = Arc::new(Mutex::new(ProcessState::Running {
            child_waiter: process,
            pid,
            signaller,
            killed: false,
        }));
        let proc_list = Arc::new(Mutex::new(None));
        let proc_list_warm_pending = Arc::new(AtomicBool::new(false));
        let scrollback_flush_sink = terminal
            .get_config()
            .scrollback_spill_sink()
            .filter(|sink| sink.requires_scrollback_flush());

        Self {
            pane_id,
            durable_pane_id,
            ownership,
            title_metadata: Mutex::new(Arc::new(Self::capture_title_metadata(&terminal))),
            terminal: Arc::new(Mutex::new(terminal)),
            cold_viewport_pending: Arc::new(Mutex::new(None)),
            cold_viewport_retry: Arc::new(AtomicBool::new(false)),
            cold_viewport_failure: Arc::new(Mutex::new(None)),
            line_layout_observation: Arc::new(Mutex::new(None)),
            output_application: Mutex::new(()),
            scrollback_flush_sink: Mutex::new(scrollback_flush_sink),
            process: Arc::clone(&process),
            pty: Arc::new(Mutex::new(pty)),
            guardian_live_output_reader: Mutex::new(guardian_live_output_reader),
            guardian_restored_prefix: Mutex::new(None),
            guardian_checkpoint_publisher,
            resize_queue: Arc::new(Mutex::new(ResizeQueueState::default())),
            writer: Mutex::new(writer),
            domain_id,
            tmux_domain,
            mux_registration,
            child_exit_prune,
            proc_list: Arc::clone(&proc_list),
            proc_list_prime_started: AtomicBool::new(false),
            proc_list_warm_pending,
            #[cfg(unix)]
            leader: Arc::new(Mutex::new(None)),
            command_description,
            #[cfg(feature = "disruptor-pane-io")]
            action_ring: Arc::new(ArrayQueue::new(PANE_ACTION_RING_CAPACITY)),
        }
    }

    #[cfg(unix)]
    fn get_leader(&self, policy: CachePolicy) -> CachedLeaderInfo {
        let mut leader = self.leader.lock();

        if policy == CachePolicy::FetchImmediate {
            leader.replace(CachedLeaderInfo::new(self.pty.lock().as_raw_fd()));
        } else if let Some(info) = leader.as_mut() {
            // If stale, queue up some work in another thread to update.
            // Right now, we'll return the stale data.
            if info.expired() && info.can_update() {
                info.updating = true;
                let leader_ref = Arc::clone(&self.leader);
                let spawn_result = std::thread::Builder::new()
                    .name(format!("pane-leader-refresh-{}", self.pane_id))
                    .spawn(move || {
                        let mut leader = leader_ref.lock();
                        if let Some(leader) = leader.as_mut() {
                            leader.update();
                        }
                    });

                if let Err(err) = spawn_result {
                    log::warn!(
                        "failed to spawn leader refresh thread pane_id={} error={err:#}; refreshing synchronously",
                        self.pane_id
                    );
                    if let Some(info) = leader.as_mut() {
                        info.updating = false;
                        info.update();
                    }
                }
            }
        } else {
            leader.replace(CachedLeaderInfo::new(self.pty.lock().as_raw_fd()));
        }

        match (*leader).clone() {
            Some(info) => info,
            None => {
                log::warn!("CachedLeaderInfo missing after refresh; rebuilding synchronously");
                CachedLeaderInfo::new(self.pty.lock().as_raw_fd())
            }
        }
    }

    fn divine_current_working_dir(&self, policy: CachePolicy) -> Option<Url> {
        #[cfg(unix)]
        {
            let leader = self.get_leader(policy);
            if let Some(path) = &leader.current_working_dir {
                return Url::from_directory_path(path).ok();
            }
            return None;
        }

        #[cfg(windows)]
        if let Some(fg) = self.divine_foreground_process(policy) {
            return Url::from_directory_path(fg.cwd).ok();
        }

        #[allow(unreachable_code)]
        None
    }

    fn divine_process_list(
        &self,
        policy: CachePolicy,
    ) -> Option<MappedMutexGuard<'_, CachedProcInfo>> {
        if let ProcessState::Running { pid: Some(pid), .. } = &*self.process.lock() {
            let mut proc_list = self.proc_list.lock();

            let expired = policy == CachePolicy::FetchImmediate
                || proc_list
                    .as_ref()
                    .map(|info| info.updated.elapsed() > PROC_INFO_CACHE_TTL)
                    .unwrap_or(true);

            if expired {
                log::trace!("CachedProcInfo expired, refresh");
                let root = LocalProcessInfo::with_root_pid(*pid)?;

                // Windows doesn't have any job control or session concept,
                // so we infer that the equivalent to the process group
                // leader is the most recently spawned program running
                // in the console. See `find_youngest_descendant`.
                let mut foreground = find_youngest_descendant(&root).clone();
                foreground.children.clear();

                proc_list.replace(CachedProcInfo {
                    root,
                    foreground,
                    updated: Instant::now(),
                    cached_is_stateful: None,
                });
                log::trace!("CachedProcInfo updated");
            }

            return Some(MutexGuard::map(proc_list, |info| info.as_mut().unwrap()));
        }
        None
    }

    #[allow(dead_code)]
    fn divine_foreground_process(&self, policy: CachePolicy) -> Option<LocalProcessInfo> {
        if let Some(info) = self.divine_process_list(policy) {
            Some(info.foreground.clone())
        } else {
            None
        }
    }

    /// Starts the opportunistic process-cache prime after mux publication.
    ///
    /// Starting from `mux_registration_did_bind` avoids guessing how long mux
    /// publication will take and gives the worker an exact generation handle.
    /// The short delay still lets a freshly spawned shell fork its initial
    /// subprocesses. If a user-driven close warm wins the single-flight race,
    /// that fresher work supersedes the prime.
    fn spawn_proc_list_prime(&self, registration: PaneRegistrationHandle) {
        let pid_for_prime = match &*self.process.lock() {
            ProcessState::Running { pid: Some(pid), .. } => *pid,
            _ => return,
        };
        if self
            .proc_list_prime_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }

        let pane_id = registration.pane_id();
        let process = Arc::clone(&self.process);
        let proc_list = Arc::clone(&self.proc_list);
        let warm_pending = Arc::clone(&self.proc_list_warm_pending);
        let spawn_result = std::thread::Builder::new()
            .name(format!("pane-proc-prime-{pane_id}"))
            .spawn(move || {
                std::thread::sleep(Duration::from_millis(250));
                let Some(_pending_guard) = ProcListWarmPendingGuard::try_acquire(&warm_pending)
                else {
                    return;
                };
                Self::warm_proc_cache(registration, pid_for_prime, process, proc_list);
            });
        if let Err(err) = spawn_result {
            self.proc_list_prime_started.store(false, Ordering::Release);
            log::warn!("failed to spawn process-cache prime pane_id={pane_id} error={err:#}");
        }
    }

    /// Single-flight background warm of `proc_list`. Spawns a worker thread
    /// that does the slow `proc_listallpids` walk off the main thread,
    /// writing the result into the cache so the next
    /// `can_close_without_prompting` call hits the fast path. No-op when a
    /// warm is already in flight or when the pane has no live process.
    /// The actual work runs in `Self::warm_proc_cache`.
    /// See ft-qhwpq.
    fn spawn_proc_list_warm(&self) {
        let Some(pending_guard) =
            ProcListWarmPendingGuard::try_acquire(&self.proc_list_warm_pending)
        else {
            return;
        };

        let pid_walked = match &*self.process.lock() {
            ProcessState::Running { pid: Some(pid), .. } => *pid,
            _ => return,
        };

        let Some(registration) = self.mux_registration.load() else {
            return;
        };
        let pane_id = registration.pane_id();
        let process = Arc::clone(&self.process);
        let proc_list = Arc::clone(&self.proc_list);
        let spawn_result = std::thread::Builder::new()
            .name(format!("pane-proc-warm-{pane_id}"))
            .spawn(move || {
                let _pending_guard = pending_guard;
                Self::warm_proc_cache(registration, pid_walked, process, proc_list);
            });
        if let Err(err) = spawn_result {
            // `Builder::spawn` drops the rejected closure, so its pending guard
            // has already released the single-flight flag.
            log::warn!("failed to spawn process-cache warm pane_id={pane_id} error={err:#}");
        }
    }

    /// Off-main-thread proc-tree walk + cache write for a specific pane.
    ///
    /// The worker carries the exact registration captured at admission. A
    /// removed or same-ID replacement pane is therefore a no-op, even if the
    /// process-tree walk completes much later. The current process PID is also
    /// checked before committing so an in-place respawn cannot receive stale
    /// metadata.
    /// See ft-qhwpq.
    fn warm_proc_cache(
        registration: PaneRegistrationHandle,
        pid_walked: u32,
        process: Arc<Mutex<ProcessState>>,
        proc_list: Arc<Mutex<Option<CachedProcInfo>>>,
    ) {
        let pane_id = registration.pane_id();
        let admitted = registration
            .try_with_current(|_| {
                let pid_now = match &*process.lock() {
                    ProcessState::Running { pid: Some(pid), .. } => Some(*pid),
                    _ => None,
                };
                if pid_now != Some(pid_walked) {
                    log::trace!(
                        "warm_proc_cache: pid changed before process walk \
                         ({pid_walked} -> {pid_now:?}) for pane \
                         {pane_id}; skipping cache refresh"
                    );
                    return false;
                }
                true
            })
            .unwrap_or(false);
        if !admitted {
            return;
        }

        // This O(N_system_processes) walk intentionally runs outside the exact
        // registration operation lease. The second admission below rejects its
        // result if removal, replacement, or an in-place respawn raced the walk.
        let Some(root) = LocalProcessInfo::with_root_pid(pid_walked) else {
            return;
        };
        let _ = registration.try_with_current(|_| {
            let pid_now = match &*process.lock() {
                ProcessState::Running { pid: Some(pid), .. } => Some(*pid),
                _ => None,
            };
            if pid_now != Some(pid_walked) {
                log::trace!(
                    "warm_proc_cache: pid changed \
                     ({pid_walked} -> {pid_now:?}) for pane \
                     {pane_id}; dropping cache write"
                );
                return;
            }

            // Build foreground identically to divine_process_list so the
            // Windows `divine_current_working_dir(&fg.cwd)` path stays correct
            // when this off-main-thread warmer populates the cache.
            let mut foreground = find_youngest_descendant(&root).clone();
            foreground.children.clear();
            proc_list.lock().replace(CachedProcInfo {
                root,
                foreground,
                updated: Instant::now(),
                cached_is_stateful: None,
            });
        });
    }
}

impl Drop for LocalPane {
    fn drop(&mut self) {
        if let Some(pending) = self.cold_viewport_pending.lock().take() {
            pending.cancelled.store(true, Ordering::Release);
        }
        let tmux_domain = self.tmux_domain.lock().take();
        if let Some(tmux) = tmux_domain {
            // Eagerly tear down tmux-domain state if this pane is being dropped
            // without a clean control-mode exit sequence.
            tmux.transition_to_exit_and_schedule_detach();
        }

        if self.ownership.retire_on_drop(self.pane_id) {
            return;
        }

        // Avoid lingering zombies if we can, but don't block forever.
        // <https://github.com/wezterm/wezterm/issues/558>
        if let ProcessState::Running { signaller, .. } = &mut *self.process.lock() {
            let _ = signaller.kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use frankenterm_term::config::ScrollbackSpillSink;
    use std::sync::atomic::AtomicUsize;

    #[derive(Debug, Default)]
    struct ColdResizeTestSink {
        rows: Mutex<(
            frankenterm_term::config::ScrollbackIntervalIdentity,
            BTreeMap<StableRowIndex, Line>,
        )>,
        busy: AtomicBool,
        usage_busy: AtomicBool,
        usage_unsupported: AtomicBool,
        unavailable: AtomicBool,
        busy_observations: AtomicUsize,
        witness_admission: AtomicBool,
        payload_reads: AtomicUsize,
        read_gate: Mutex<Option<(std::sync::mpsc::SyncSender<()>, Receiver<()>)>>,
    }

    impl frankenterm_term::config::ScrollbackSpillSink for ColdResizeTestSink {
        fn try_capture_scrollback_interval(
            &self,
        ) -> frankenterm_term::config::ScrollbackIntervalCapture {
            use frankenterm_term::config::ScrollbackIntervalCapture;
            if self.unavailable.load(Ordering::Acquire) {
                return ScrollbackIntervalCapture::Unavailable;
            }
            if self.busy.load(Ordering::Acquire) {
                self.busy_observations.fetch_add(1, Ordering::Release);
                return ScrollbackIntervalCapture::Busy;
            }
            let Some(rows) = self.rows.try_lock() else {
                return ScrollbackIntervalCapture::Busy;
            };
            rows.0.capture(
                rows.1
                    .first_key_value()
                    .zip(rows.1.last_key_value())
                    .map(|((&first, _), (&last, _))| first..last + 1),
            )
        }

        fn try_capture_scrollback_usage(&self) -> frankenterm_term::config::ScrollbackUsageCapture {
            use frankenterm_term::config::ScrollbackUsageCapture;
            if self.usage_unsupported.load(Ordering::Acquire) {
                return ScrollbackUsageCapture::Unsupported;
            }
            if self.unavailable.load(Ordering::Acquire) {
                return ScrollbackUsageCapture::Unavailable;
            }
            if self.busy.load(Ordering::Acquire) || self.usage_busy.load(Ordering::Acquire) {
                self.busy_observations.fetch_add(1, Ordering::Release);
                return ScrollbackUsageCapture::Busy;
            }
            let Some(rows) = self.rows.try_lock() else {
                return ScrollbackUsageCapture::Busy;
            };
            let count = rows.1.len();
            let bytes = rows
                .1
                .values()
                .map(|line| line.len() * std::mem::size_of::<termwiz::cell::Cell>())
                .sum();
            ScrollbackUsageCapture::Ready(frankenterm_term::config::ScrollbackUsage {
                rows: count,
                bytes,
            })
        }

        fn store_scrollback_line(&self, row: StableRowIndex, line: &Line, limit: usize) -> bool {
            self.store_scrollback_line_with_receipt(row, line, limit)
                .accepted()
        }

        fn store_scrollback_line_with_receipt(
            &self,
            row: StableRowIndex,
            line: &Line,
            limit: usize,
        ) -> frankenterm_term::config::ScrollbackLineAdmission {
            use frankenterm_term::config::{ScrollbackIntervalCapture, ScrollbackLineAdmission};
            let mut rows = self.rows.lock();
            if limit == 0 || rows.1.len() >= limit {
                return ScrollbackLineAdmission::Refused;
            }
            if rows.1.insert(row, line.clone()).is_some() {
                rows.0 = Default::default();
            }
            let interval = if self.witness_admission.load(Ordering::Relaxed) {
                let first = *rows.1.first_key_value().unwrap().0;
                let end = rows.1.last_key_value().unwrap().0.checked_add(1);
                match end.map(|end| rows.0.capture(Some(first..end))) {
                    Some(ScrollbackIntervalCapture::Ready(interval)) => Some(interval),
                    _ => None,
                }
            } else {
                None
            };
            ScrollbackLineAdmission::Admitted { interval }
        }

        fn load_scrollback_line(&self, row: StableRowIndex) -> Option<Line> {
            self.payload_reads.fetch_add(1, Ordering::Relaxed);
            let gate = self.read_gate.lock().take();
            if let Some((entered, release)) = gate {
                entered.send(()).unwrap();
                release.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            self.rows.lock().1.get(&row).cloned()
        }

        fn oldest_scrollback_row(&self) -> Option<StableRowIndex> {
            self.rows.lock().1.first_key_value().map(|(&row, _)| row)
        }
        fn retained_scrollback_rows(&self) -> usize {
            self.rows.lock().1.len()
        }
        fn retained_scrollback_bytes(&self) -> usize {
            self.rows
                .lock()
                .1
                .values()
                .map(|line| line.len() * std::mem::size_of::<termwiz::cell::Cell>())
                .sum()
        }

        fn snapshot_scrollback(
            &self,
            _: StableRowIndex,
            _: frankenterm_term::config::ScrollbackSnapshotLimits,
        ) -> Result<
            frankenterm_term::config::ScrollbackSnapshot,
            frankenterm_term::config::ScrollbackSpillError,
        > {
            panic!("cold resize must not snapshot the entire durable history")
        }

        fn replace_scrollback_prefix(
            &self,
            _: Option<frankenterm_term::config::ScrollbackSnapshotGeneration>,
            _: frankenterm_term::config::ScrollbackPrefix<'_>,
            _: usize,
        ) -> Result<
            frankenterm_term::config::ScrollbackReplaceCommit,
            frankenterm_term::config::ScrollbackSpillError,
        > {
            panic!("cold resize must preserve durable storage keys and payloads")
        }

        fn clear_scrollback(
            &self,
        ) -> Result<
            frankenterm_term::config::ScrollbackClearCommit,
            frankenterm_term::config::ScrollbackSpillError,
        > {
            panic!("cold resize must not clear durable history")
        }
    }

    #[derive(Debug)]
    struct ColdResizeTestConfig(Arc<ColdResizeTestSink>);

    impl TerminalConfiguration for ColdResizeTestConfig {
        fn color_palette(&self) -> ColorPalette {
            ColorPalette::default()
        }
        fn scrollback_size(&self) -> usize {
            32
        }
        fn scrollback_tier_config(&self) -> frankenterm_term::config::ScrollbackTierConfig {
            frankenterm_term::config::ScrollbackTierConfig {
                enabled: true,
                hot_lines: 1,
                warm_max_bytes: 0,
            }
        }
        fn scrollback_spill_sink(
            &self,
        ) -> Option<Arc<dyn frankenterm_term::config::ScrollbackSpillSink>> {
            Some(self.0.clone())
        }
    }

    #[derive(Clone, Debug, Default)]
    struct ColdResizeTestChild {
        exit: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
    }

    impl ChildKiller for ColdResizeTestChild {
        fn kill(&mut self) -> IoResult<()> {
            *self.exit.0.lock().unwrap() = true;
            self.exit.1.notify_all();
            Ok(())
        }

        fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
            Box::new(self.clone())
        }
    }

    impl Child for ColdResizeTestChild {
        fn try_wait(&mut self) -> IoResult<Option<ExitStatus>> {
            // Window publication legitimately prunes exited panes. Resize
            // fixtures must represent a shell that is still running.
            Ok(self
                .exit
                .0
                .lock()
                .unwrap()
                .then_some(ExitStatus::with_exit_code(0)))
        }

        fn wait(&mut self) -> IoResult<ExitStatus> {
            let mut exited = self.exit.0.lock().unwrap();
            while !*exited {
                exited = self.exit.1.wait(exited).unwrap();
            }
            Ok(ExitStatus::with_exit_code(0))
        }

        fn process_id(&self) -> Option<u32> {
            None
        }
    }

    fn cold_resize_fixture(
        witness_admission: bool,
    ) -> (
        Arc<LocalPane>,
        Arc<crate::Mux>,
        PaneRegistrationHandle,
        Arc<ColdResizeTestSink>,
        ResizeCancellationToken,
    ) {
        let sink = Arc::new(ColdResizeTestSink::default());
        sink.witness_admission
            .store(witness_admission, Ordering::Relaxed);
        let mut term = Terminal::new(
            term_size(4, 2),
            Arc::new(ColdResizeTestConfig(sink.clone())),
            "FrankenTerm",
            "cold-resize-test",
            Box::new(Vec::new()),
        );
        term.advance_bytes(b"abcdefgh\r\none\r\n");
        term.resize(term_size(3, 2));
        assert!(term.screen().capture_cold_seam_reflow().unwrap().is_some());
        let domain: Arc<dyn Domain> =
            Arc::new(crate::domain::LocalDomain::new("cold-resize-test").unwrap());
        let pane = Arc::new(LocalPane::new(
            719,
            term,
            Box::new(ColdResizeTestChild::default()),
            Box::new(GuardianLifetimeTestMasterPty),
            Box::new(Vec::<u8>::new()),
            domain.domain_id(),
            [0x79; 16],
            "cold-resize-test".to_string(),
        ));
        let registered: Arc<dyn Pane> = pane.clone();
        let mux = Arc::new(crate::Mux::new(Some(domain)));
        let generation = crate::PaneRegistrationGeneration::new(
            pane.pane_id(),
            &mux.pane_retirements,
            Arc::downgrade(&mux),
        );
        {
            let _registration = mux.pane_registration.lock();
            mux.insert_pane_registration_locked(
                pane.pane_id(),
                pane.domain_id(),
                &registered,
                &generation,
            )
            .unwrap();
        }
        let registration = mux.capture_pane_registration(&registered).unwrap();
        pane.mux_registration
            .reserve(registration.clone())
            .unwrap()
            .commit()
            .unwrap()
            .finalize();
        let mut queue = pane.resize_queue.lock();
        let outcome = queue.enqueue(term_size(3, 2), pty_size(3, 2), Instant::now());
        let pending = queue.dequeue_for_worker().unwrap();
        assert_eq!(pending.seq, outcome.seq);
        drop(queue);
        (
            pane,
            mux,
            registration,
            sink,
            ResizeCancellationToken::new(outcome.seq),
        )
    }

    #[test]
    fn line_layout_preserves_busy_and_advances_on_real_eviction() {
        let (pane, _mux, _registration, sink, _token) = cold_resize_fixture(false);
        let (floor, dimensions) = pane.get_line_layout().unwrap().unwrap();
        assert!(dimensions.scrollback_rows > 0);
        let locked_rows = sink.rows.lock();
        assert_eq!(
            pane.get_line_layout(),
            Err(frankenterm_term::screen::ColdReadMetadataBusy)
        );
        assert!(pane.selection_source_snapshot().is_none());
        assert_eq!(
            pane.publish_line_reads_at_layout(&[], floor, dimensions, &mut || ()),
            Err(frankenterm_term::screen::ColdReadMetadataBusy)
        );
        assert_eq!(
            pane.get_dimensions().scrollback_top,
            dimensions.scrollback_top
        );
        drop(locked_rows);
        let (_, released) = pane.get_line_layout().unwrap().unwrap();
        assert_eq!(released.scrollback_top, dimensions.scrollback_top);
        let oldest = {
            let mut rows = sink.rows.lock();
            let oldest = *rows.1.first_key_value().unwrap().0;
            rows.1.remove(&oldest);
            oldest
        };
        assert!(pane.get_dimensions().scrollback_top > oldest);
        let (_, evicted) = pane.get_line_layout().unwrap().unwrap();
        assert!(evicted.scrollback_top > oldest);
        sink.unavailable.store(true, Ordering::Release);
        assert_eq!(pane.get_line_layout(), Ok(None));
    }

    fn cold_resize_read_all(pane: &LocalPane) -> anyhow::Result<String> {
        let read = {
            let term = pane.terminal.lock();
            let (first, rows) = term.screen().scrollback_geometry();
            term.screen()
                .capture_line_read(first..first + rows as StableRowIndex)?
        }
        .hydrate(|| false)?;
        anyhow::ensure!(
            pane.terminal.lock().screen().validates_line_read(&read),
            "test read must retain exact source"
        );
        let mut text = String::new();
        for line in read.lines() {
            text.push_str(&line.as_str());
            if !line.last_cell_was_wrapped() {
                text.push('\n');
            }
        }
        Ok(text)
    }

    #[test]
    fn native_resize_completion_notifies_without_cold_layout_and_fences_stale_work() {
        const CHILD: &str = "FT_RESIZE_COMPLETION_NOTIFY_CHILD";
        if std::env::var_os(CHILD).as_deref() != Some(std::ffi::OsStr::new("1")) {
            // The actual main-thread scheduler is process-global. Isolate its
            // installation so this causal notification test cannot reroute
            // another concurrently running mux test's callbacks.
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "localpane::tests::native_resize_completion_notifies_without_cold_layout_and_fences_stale_work",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                if child.try_wait().unwrap().is_some() {
                    let output = child.wait_with_output().unwrap();
                    assert!(
                        output.status.success(),
                        "notification subprocess failed: {}{}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr),
                    );
                    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
                    return;
                }
                if Instant::now() >= deadline {
                    child.kill().unwrap();
                    let output = child.wait_with_output().unwrap();
                    panic!(
                        "notification subprocess exceeded watchdog: {}{}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr),
                    );
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        let executor = promise::spawn::SimpleExecutor::try_with_limits(
            promise::spawn::MainThreadAdmissionLimits::new(8, 128 * 1024, 0, 0).unwrap(),
        )
        .unwrap();
        for case in [
            "no_cold",
            "unavailable",
            "saturated",
            "inline_fallback",
            "local_geometry",
            "superseded",
            "retired",
            "late_superseded",
            "late_retired",
        ] {
            let (pane, mux, registration, sink, token) = cold_resize_fixture(false);
            pane.resize_queue.lock().reconcile_tab_on_completion = case != "local_geometry";
            if case != "local_geometry" && case != "inline_fallback" {
                let reservation = match promise::spawn::try_reserve_main_thread(
                    promise::spawn::MainThreadServiceClass::Topology,
                    LOCAL_PANE_MAIN_THREAD_ESTIMATED_BYTES,
                ) {
                    promise::spawn::MainThreadReservationOutcome::Reserved(reservation) => {
                        reservation
                    }
                    other => panic!("fixture completion admission failed: {:?}", other),
                };
                pane.resize_queue.lock().completion_reservation = Some(reservation);
            }
            // Reproduce the server tab still carrying geometry observed just
            // after asynchronous resize admission, before the pane committed.
            let tab = Arc::new(crate::tab::Tab::new(&term_size(4, 2)));
            mux.add_tab_no_panes(&tab).unwrap();
            let window = mux.new_empty_window(None, None);
            mux.add_tab_to_window(&tab, *window).unwrap();
            // Publish window creation before measuring resize completion;
            // the builder otherwise queues its notification when dropped at
            // the end of this iteration, after the scheduler was drained.
            drop(window);
            let dynamic_pane: Arc<dyn Pane> = pane.clone();
            tab.assign_pane(&dynamic_pane);
            assert_eq!(tab.get_size(), term_size(4, 2));
            if case == "no_cold" {
                let mut term = guardian_lifetime_test_terminal();
                term.resize(term_size(3, 2));
                *pane.terminal.lock() = term;
            } else {
                sink.unavailable.store(true, Ordering::Release);
            }
            let notifications = Arc::new(AtomicUsize::new(0));
            let observed = Arc::clone(&notifications);
            mux.subscribe(move |notification| {
                if matches!(notification, crate::MuxNotification::PaneOutput(719)) {
                    observed.fetch_add(1, Ordering::Relaxed);
                }
                true
            })
            .unwrap();
            if case == "inline_fallback" {
                // The fixture already owns worker admission, so this queues
                // a real remote intent without creating another OS thread.
                pane.resize_from_remote(term_size(3, 2)).unwrap();
            }
            let mut held_capacity = Vec::new();
            if matches!(case, "saturated" | "inline_fallback") {
                loop {
                    match promise::spawn::try_reserve_main_thread(
                        promise::spawn::MainThreadServiceClass::Topology,
                        LOCAL_PANE_MAIN_THREAD_ESTIMATED_BYTES,
                    ) {
                        promise::spawn::MainThreadReservationOutcome::Reserved(reservation) => {
                            held_capacity.push(reservation);
                        }
                        promise::spawn::MainThreadReservationOutcome::RetryableFull(_) => break,
                        other => panic!("unexpected saturation result: {:?}", other),
                    }
                    assert!(held_capacity.len() <= 8);
                }
                let admitted_seq = pane.resize_queue.lock().next_seq;
                assert!(pane.resize_from_remote(term_size(5, 2)).is_err());
                assert_eq!(pane.resize_queue.lock().next_seq, admitted_seq);
                assert_eq!(pane.get_dimensions().cols, 3);
            }
            let supersede = || {
                pane.resize_queue
                    .lock()
                    .enqueue(term_size(5, 2), pty_size(5, 2), Instant::now());
            };
            if case == "superseded" {
                supersede();
            } else if case == "retired" {
                registration.retire_if_current();
            }
            if case == "inline_fallback" {
                settle_resize_worker_spawn(Err::<(), ()>(()), || {
                    LocalPane::run_resize_worker(
                        pane.pane_id(),
                        Arc::clone(&pane.terminal),
                        Arc::clone(&pane.line_layout_observation),
                        #[cfg(feature = "disruptor-pane-io")]
                        Arc::clone(&pane.action_ring),
                        Arc::clone(&pane.pty),
                        Arc::clone(&pane.resize_queue),
                        Arc::clone(&pane.mux_registration),
                        false,
                    );
                });
            } else {
                LocalPane::prepare_cold_layout_after_resize(
                    pane.pane_id(),
                    &pane.terminal,
                    &pane.line_layout_observation,
                    &pane.resize_queue,
                    token,
                    registration.clone(),
                );
            }
            if case == "late_superseded" {
                supersede();
            } else if case == "late_retired" {
                registration.retire_if_current();
            }
            let mut dispatched = 0;
            while executor.try_tick().unwrap() {
                dispatched += 1;
                assert!(dispatched <= 32, "resize wakeup must have bounded fanout");
            }
            assert_eq!(
                notifications.load(Ordering::Relaxed),
                usize::from(matches!(
                    case,
                    "no_cold" | "unavailable" | "saturated" | "inline_fallback" | "local_geometry"
                )),
                "{case}: primary completion must wake exactly its current pane",
            );
            assert_eq!(
                tab.get_size(),
                term_size(
                    if matches!(
                        case,
                        "no_cold" | "unavailable" | "saturated" | "inline_fallback"
                    ) {
                        3
                    } else {
                        4
                    },
                    2,
                ),
                "{case}: only current completion may reconcile the containing tab",
            );
            assert!(pane.resize_queue.lock().completion_reservation.is_none());
            drop(held_capacity);
            assert_eq!(executor.admission_snapshot().active_tasks, 0, "{case}");
        }

        for failure in [
            ResizeFailureKind::RecoverablePanic,
            ResizeFailureKind::ApplyError,
        ] {
            let reservation = match promise::spawn::try_reserve_main_thread(
                promise::spawn::MainThreadServiceClass::Topology,
                LOCAL_PANE_MAIN_THREAD_ESTIMATED_BYTES,
            ) {
                promise::spawn::MainThreadReservationOutcome::Reserved(reservation) => reservation,
                other => panic!("retry fixture admission failed: {:?}", other),
            };
            let mut queue = ResizeQueueState::default();
            queue
                .try_enqueue(
                    term_size(3, 2),
                    pty_size(3, 2),
                    Instant::now(),
                    true,
                    Some(reservation),
                )
                .unwrap();
            let mut pending = queue.dequeue_for_worker().unwrap();
            for retry in 1..=failure.retry_limit() {
                assert_eq!(
                    queue.recover_failed_intent(pending, failure),
                    ResizeFailureRecovery::Requeued { retry },
                );
                assert_eq!(executor.admission_snapshot().active_tasks, 1);
                pending = queue.dequeue_for_worker().unwrap();
            }
            assert!(matches!(
                queue.recover_failed_intent(pending, failure),
                ResizeFailureRecovery::ExhaustedRetained { .. },
            ));
            assert!(
                queue.pending.is_some(),
                "failed target must remain retained"
            );
            assert!(queue.completion_reservation.is_none());
            assert_eq!(executor.admission_snapshot().active_tasks, 0);
        }

        // Exercise the actual child-waiter bridge, including its late exit
        // after retirement. Stale fixture teardown must not allocate a task
        // that contaminates a later resize owner's exact permit accounting.
        for retired in [false, true] {
            let (pane, mux, registration, _sink, _token) = cold_resize_fixture(false);
            let weak_owner = Arc::downgrade(&mux);
            let prune = Arc::clone(&pane.child_exit_prune);
            if retired {
                assert!(registration.retire_if_current());
            } else {
                pane.kill();
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let child_exited = prune.tracker.lock().child_exited;
                if child_exited && (retired || executor.queue_snapshot().depth != 0) {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "child waiter failed to publish its exit"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            // Also cover a subsequent is_dead-style retry of that same slot.
            prune.try_schedule();
            assert_eq!(
                executor.admission_snapshot().active_tasks,
                usize::from(!retired)
            );
            assert_eq!(executor.queue_snapshot().depth, usize::from(!retired));
            let mut dispatched = 0;
            while executor.try_tick().unwrap() {
                dispatched += 1;
                assert!(dispatched <= 32, "child prune must have bounded fanout");
            }
            assert_eq!(prune.tracker.lock().has_pending_intent(), retired);
            assert_eq!(executor.admission_snapshot().active_tasks, 0);
            drop(registration);
            drop(pane);
            drop(mux);
            assert!(
                weak_owner.upgrade().is_none(),
                "retained exit intent must not retain its mux"
            );
        }
    }

    #[test]
    fn tab_geometry_completion_dispatches_while_cold_preparation_is_gated() {
        const CHILD: &str = "FT_RESIZE_COMPLETION_GATED_CHILD";
        if std::env::var_os(CHILD).as_deref() != Some(std::ffi::OsStr::new("1")) {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "localpane::tests::tab_geometry_completion_dispatches_while_cold_preparation_is_gated",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                if child.try_wait().unwrap().is_some() {
                    let output = child.wait_with_output().unwrap();
                    assert!(
                        output.status.success(),
                        "notification subprocess failed: {}{}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr),
                    );
                    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
                    return;
                }
                if Instant::now() >= deadline {
                    child.kill().unwrap();
                    let output = child.wait_with_output().unwrap();
                    panic!(
                        "notification subprocess exceeded watchdog: {}{}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr),
                    );
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        let executor = promise::spawn::SimpleExecutor::try_with_limits(
            promise::spawn::MainThreadAdmissionLimits::new(8, 128 * 1024, 0, 0).unwrap(),
        )
        .unwrap();

        let (pane, mux, _registration, sink, token) = cold_resize_fixture(false);

        let tab = Arc::new(crate::tab::Tab::new(&term_size(4, 2)));
        mux.add_tab_no_panes(&tab).unwrap();
        let window = mux.new_empty_window(None, None);
        mux.add_tab_to_window(&tab, *window).unwrap();
        drop(window);
        let dynamic_pane: Arc<dyn Pane> = pane.clone();
        tab.assign_pane(&dynamic_pane);
        assert_eq!(tab.get_size(), term_size(4, 2));

        // Settle cold seam first so read_gate cleanly isolates cold index preparation
        let seam = {
            let term = pane.terminal.lock();
            term.screen().capture_cold_seam_reflow().unwrap().unwrap()
        };
        let mut seam = seam.hydrate(|| false).unwrap();
        {
            let mut term = pane.terminal.lock();
            let seqno = next_cold_resize_sequence(term.current_seqno()).unwrap();
            assert!(term
                .screen_mut()
                .install_cold_seam_reflow(&mut seam, seqno)
                .unwrap());
            term.increment_seqno();
            assert!(LocalPane::publish_resize_source(
                pane.pane_id(),
                &pane.line_layout_observation,
                &mut term,
                token,
                token.seq,
                "cold_seam",
            ));
        }
        drop(seam);

        // Queue a remote resize to (3, 2). The fixture already owns worker admission,
        // so this queues a real remote intent without creating another OS thread.
        pane.resize_from_remote(term_size(3, 2)).unwrap();
        assert!(pane.resize_queue.lock().completion_reservation.is_some());
        assert!(pane.resize_queue.lock().reconcile_tab_on_completion);

        // Gate cold preparation via sink.read_gate
        let (entered_tx, entered_rx) = sync_channel(1);
        let (release_tx, release_rx) = sync_channel(1);
        *sink.read_gate.lock() = Some((entered_tx, release_rx));

        // Spawn worker running actual run_resize_worker with gated cold preparation
        let target = Arc::clone(&pane);
        let (worker_done_tx, worker_done_rx) = sync_channel(1);
        let worker = std::thread::spawn(move || {
            LocalPane::run_resize_worker(
                target.pane_id(),
                Arc::clone(&target.terminal),
                Arc::clone(&target.line_layout_observation),
                #[cfg(feature = "disruptor-pane-io")]
                Arc::clone(&target.action_ring),
                Arc::clone(&target.pty),
                Arc::clone(&target.resize_queue),
                Arc::clone(&target.mux_registration),
                true,
            );
            worker_done_tx.send(()).unwrap();
        });

        // Worker enters cold preparation and suspends on read_gate
        let entered = entered_rx.recv_timeout(Duration::from_secs(5));
        assert!(entered.is_ok(), "worker must enter gated cold preparation");

        // Worker is still suspended; cold preparation has NOT completed
        assert!(
            worker_done_rx.try_recv().is_err(),
            "worker must still be suspended in cold preparation"
        );

        // Causal check: tab geometry completion has already been dispatched to the executor
        let mut dispatched = 0;
        while executor.try_tick().unwrap() {
            dispatched += 1;
            assert!(dispatched <= 32);
        }
        assert!(
            dispatched > 0,
            "tab geometry completion must dispatch while cold preparation is gated"
        );

        // Tab size is now reconciled to (3, 2) WHILE worker is still suspended in cold preparation
        assert_eq!(
            tab.get_size(),
            term_size(3, 2),
            "tab geometry must be reconciled while cold preparation is still gated"
        );

        // Pre-admitted permit was consumed
        assert!(pane.resize_queue.lock().completion_reservation.is_none());

        // Worker remains suspended until gate is released
        assert!(
            worker_done_rx.try_recv().is_err(),
            "worker must remain suspended until gate is released"
        );

        // Release gate and join worker
        let _ = release_tx.send(());
        let worker_finished = worker_done_rx.recv_timeout(Duration::from_secs(5));
        worker.join().unwrap();
        worker_finished.unwrap();

        // Drain any remaining cold-ready wakeups
        while executor.try_tick().unwrap() {}
    }

    #[test]
    fn cold_resize_geometry_publication_skips_payload_and_preserves_fallback() {
        for witnessed in [true, false] {
            let (pane, _mux, registration, sink, token) = cold_resize_fixture(witnessed);
            // The seam legitimately needs its crossing paragraph. Settle it
            // first so the counter isolates the production index phase.
            let seam = {
                let term = pane.terminal.lock();
                term.screen().capture_cold_seam_reflow().unwrap().unwrap()
            };
            let mut seam = seam.hydrate(|| false).unwrap();
            {
                let mut term = pane.terminal.lock();
                let seqno = next_cold_resize_sequence(term.current_seqno()).unwrap();
                assert!(term
                    .screen_mut()
                    .install_cold_seam_reflow(&mut seam, seqno)
                    .unwrap());
                term.increment_seqno();
                assert!(LocalPane::publish_resize_source(
                    pane.pane_id(),
                    &pane.line_layout_observation,
                    &mut term,
                    token,
                    token.seq,
                    "cold_seam",
                ));
            }
            drop(seam);
            let before = pane
                .try_capture_render_frame(None, 0, 0, &[], false)
                .unwrap();
            sink.payload_reads.store(0, Ordering::Relaxed);
            let target = Arc::clone(&pane);
            let (done_tx, done_rx) = sync_channel(1);
            let worker = std::thread::spawn(move || {
                LocalPane::prepare_cold_layout_after_resize(
                    target.pane_id(),
                    &target.terminal,
                    &target.line_layout_observation,
                    &target.resize_queue,
                    token,
                    registration,
                );
                done_tx.send(()).unwrap();
            });
            let settled = done_rx.recv_timeout(Duration::from_secs(5));
            if settled.is_err() {
                pane.resize_queue.lock().next_seq += 1;
            }
            worker.join().unwrap();
            settled.unwrap();
            let reads = sink.payload_reads.load(Ordering::Relaxed);
            if witnessed {
                assert_eq!(reads, 0, "admitted index must not hydrate an unused row");
            } else {
                assert!(
                    reads > 0,
                    "unwitnessed admission must retain the real payload fallback"
                );
            }
            let after = pane
                .try_capture_render_frame(
                    None,
                    before.source_sequence,
                    before.source_sequence,
                    &[],
                    false,
                )
                .unwrap();
            assert!(after.source_sequence > before.source_sequence);
            assert!(after.layout_floor > before.layout_floor);
            assert_eq!(after.first, before.first);
            assert_eq!(after.lines, before.lines);
            assert_eq!(after.cursor, before.cursor);
            assert!(
                after.dirty.is_empty(),
                "cold indexing does not damage the resident viewport"
            );
            assert_eq!(cold_resize_read_all(&pane).unwrap(), "abcdefgh\none\n\n");
            assert!(sink.payload_reads.load(Ordering::Relaxed) > 0);
        }
    }

    #[test]
    fn cold_resize_metadata_contention_settles_without_another_resize() {
        let (pane, _mux, registration, sink, token) = cold_resize_fixture(false);
        assert!(cold_resize_read_all(&pane)
            .unwrap_err()
            .is::<frankenterm_term::screen::ColdReadGeometryUnavailable>());
        sink.busy.store(true, Ordering::Release);
        let target = Arc::clone(&pane);
        let (done_tx, done_rx) = sync_channel(1);
        let worker = std::thread::spawn(move || {
            LocalPane::prepare_cold_layout_after_resize(
                target.pane_id(),
                &target.terminal,
                &target.line_layout_observation,
                &target.resize_queue,
                token,
                registration,
            );
            done_tx.send(()).unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while sink.busy_observations.load(Ordering::Acquire) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        let observed = sink.busy_observations.load(Ordering::Acquire) > 0;
        let premature = done_rx.try_recv().is_ok();
        sink.busy.store(false, Ordering::Release);
        let settled = done_rx.recv_timeout(Duration::from_secs(5));
        if settled.is_err() {
            pane.resize_queue.lock().next_seq += 1;
        }
        worker.join().unwrap();
        assert!(
            observed && !premature,
            "actual metadata contention must retain the worker"
        );
        settled.unwrap();
        assert_eq!(
            pane.resize_queue.lock().next_seq,
            token.seq,
            "no second resize repaired the seam"
        );
        assert_eq!(cold_resize_read_all(&pane).unwrap(), "abcdefgh\none\n\n");
    }

    #[test]
    fn cold_resize_recaptures_changed_resident_source_before_commit() {
        let (pane, _mux, registration, sink, token) = cold_resize_fixture(false);
        let (entered_tx, entered_rx) = sync_channel(1);
        let (release_tx, release_rx) = sync_channel(1);
        *sink.read_gate.lock() = Some((entered_tx, release_rx));
        let target = Arc::clone(&pane);
        let (done_tx, done_rx) = sync_channel(1);
        let worker = std::thread::spawn(move || {
            LocalPane::prepare_cold_layout_after_resize(
                target.pane_id(),
                &target.terminal,
                &target.line_layout_observation,
                &target.resize_queue,
                token,
                registration,
            );
            done_tx.send(()).unwrap();
        });
        let entered = entered_rx.recv_timeout(Duration::from_secs(5));
        let mut source_was_expected = false;
        if entered.is_ok() {
            let mut term = pane.terminal.lock();
            term.increment_seqno();
            let seqno = term.current_seqno();
            let line = term.screen_mut().line_mut(0);
            source_was_expected = line.as_str().starts_with('e');
            line.set_cell(
                0,
                termwiz::cell::Cell::new('Z', termwiz::cell::CellAttributes::blank()),
                seqno,
            );
        }
        let _ = release_tx.send(());
        let settled = done_rx.recv_timeout(Duration::from_secs(5));
        if settled.is_err() {
            pane.resize_queue.lock().next_seq += 1;
        }
        worker.join().unwrap();
        entered.unwrap();
        settled.unwrap();
        assert!(source_was_expected);
        assert_eq!(pane.resize_queue.lock().next_seq, token.seq);
        assert_eq!(
            cold_resize_read_all(&pane).unwrap(),
            "abcdZfgh\none\n\n",
            "old hydrated cells must not overwrite the current source or strand its seam"
        );
    }

    #[test]
    fn cold_resize_recaptures_when_output_scrolls_frontier_during_index_preparation() {
        let (pane, _mux, registration, sink, token) = cold_resize_fixture(false);
        // Settle the cold seam first so the read gate isolates index preparation.
        let seam = {
            let term = pane.terminal.lock();
            term.screen().capture_cold_seam_reflow().unwrap().unwrap()
        };
        let mut seam = seam.hydrate(|| false).unwrap();
        {
            let mut term = pane.terminal.lock();
            let seqno = next_cold_resize_sequence(term.current_seqno()).unwrap();
            assert!(term
                .screen_mut()
                .install_cold_seam_reflow(&mut seam, seqno)
                .unwrap());
            term.increment_seqno();
            assert!(LocalPane::publish_resize_source(
                pane.pane_id(),
                &pane.line_layout_observation,
                &mut term,
                token,
                token.seq,
                "cold_seam",
            ));
        }
        drop(seam);

        let (entered_tx, entered_rx) = sync_channel(1);
        let (release_tx, release_rx) = sync_channel(1);
        *sink.read_gate.lock() = Some((entered_tx, release_rx));
        let target = Arc::clone(&pane);
        let (done_tx, done_rx) = sync_channel(1);
        let worker = std::thread::spawn(move || {
            LocalPane::prepare_cold_layout_after_resize(
                target.pane_id(),
                &target.terminal,
                &target.line_layout_observation,
                &target.resize_queue,
                token,
                registration,
            );
            done_tx.send(()).unwrap();
        });

        let entered = entered_rx.recv_timeout(Duration::from_secs(5));
        assert!(entered.is_ok(), "worker must enter index preparation");

        // Worker is now suspended in prepare_cold_layout having captured the
        // initial plan at the old resident frontier.
        let frontier_before = pane.terminal.lock().screen().phys_to_stable_row_index(0);

        // Prepare a sample layout at this old frontier to verify that moving
        // the frontier deterministically invalidates it.
        let probe_plan = {
            let term = pane.terminal.lock();
            term.screen()
                .capture_line_read(StableRowIndex::MIN..StableRowIndex::MIN + 1)
                .unwrap()
        };
        let probe_prepared = probe_plan.prepare_cold_layout(|| false).unwrap();
        assert!(
            pane.terminal
                .lock()
                .screen()
                .validates_prepared_cold_layout(&probe_prepared)
                .unwrap(),
            "layout prepared at current frontier must initially validate"
        );

        // Advance bytes to write output and cause scrolling, which evicts physical row 0
        // into the spill sink and advances phys_to_stable_row_index(0).
        {
            let mut term = pane.terminal.lock();
            term.advance_bytes(b"two\r\n");
        }

        let frontier_after = pane.terminal.lock().screen().phys_to_stable_row_index(0);
        assert!(
            frontier_after > frontier_before,
            "output scroll must advance resident cold frontier"
        );

        // Negative check: prove that the layout prepared at the old frontier
        // fails validation now that the screen's resident frontier has moved.
        assert!(
            !pane
                .terminal
                .lock()
                .screen()
                .validates_prepared_cold_layout(&probe_prepared)
                .unwrap(),
            "layout prepared at stale frontier must be rejected by validates_prepared_cold_layout"
        );

        // Positive check: release the worker to complete the first pass (which
        // fails commit validation on the stale layout) and recapture with the
        // new frontier.
        let _ = release_tx.send(());
        let settled = done_rx.recv_timeout(Duration::from_secs(5));
        if settled.is_err() {
            pane.resize_queue.lock().next_seq += 1;
        }
        worker.join().unwrap();
        settled.unwrap();

        assert_eq!(pane.resize_queue.lock().next_seq, token.seq);
        assert_eq!(
            cold_resize_read_all(&pane).unwrap(),
            "abcdefgh\none\ntwo\n\n",
            "recaptured cold layout must install successfully and permit exact full-history reads"
        );
    }

    #[test]
    fn cold_resize_busy_wait_cancels_for_supersession_or_retirement() {
        for retire in [false, true] {
            let (pane, _mux, registration, sink, token) = cold_resize_fixture(false);
            sink.busy.store(true, Ordering::Release);
            let target = Arc::clone(&pane);
            let worker_registration = registration.clone();
            let (done_tx, done_rx) = sync_channel(1);
            let worker = std::thread::spawn(move || {
                LocalPane::prepare_cold_layout_after_resize(
                    target.pane_id(),
                    &target.terminal,
                    &target.line_layout_observation,
                    &target.resize_queue,
                    token,
                    worker_registration,
                );
                done_tx.send(()).unwrap();
            });
            let deadline = Instant::now() + Duration::from_secs(5);
            while sink.busy_observations.load(Ordering::Acquire) == 0 && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            let observed = sink.busy_observations.load(Ordering::Acquire) > 0;
            if retire {
                registration.retire_if_current();
            } else {
                pane.resize_queue
                    .lock()
                    .enqueue(term_size(5, 2), pty_size(5, 2), Instant::now());
            }
            let cancelled = done_rx.recv_timeout(Duration::from_secs(1));
            sink.busy.store(false, Ordering::Release);
            if cancelled.is_err() {
                pane.resize_queue.lock().next_seq += 1;
            }
            worker.join().unwrap();
            assert!(observed);
            cancelled
                .expect("supersession/retirement must end backoff without releasing busy metadata");
            assert!(
                pane.terminal
                    .lock()
                    .screen()
                    .capture_cold_seam_reflow()
                    .unwrap()
                    .is_some(),
                "cancelled old source must not publish a seam"
            );
        }
    }

    #[test]
    fn cold_resize_retry_does_not_swallow_geometry_failure() {
        let mut attempts = 0;
        let result = retry_cold_resize_step::<()>("test", &|| false, || {
            attempts += 1;
            Err(frankenterm_term::screen::ColdReadGeometryUnavailable.into())
        });
        assert_eq!(attempts, 1);
        assert!(result
            .unwrap_err()
            .is::<frankenterm_term::screen::ColdReadGeometryUnavailable>());
    }

    #[test]
    fn cold_resize_sequence_ceiling_fails_without_repeating_stale_install() {
        assert_eq!(
            next_cold_resize_sequence(SequenceNo::MAX - 2).unwrap(),
            SequenceNo::MAX - 1
        );
        for current in [SequenceNo::MAX - 1, SequenceNo::MAX] {
            let mut attempts = 0;
            let result = retry_cold_resize_step("seam_commit", &|| false, || {
                attempts += 1;
                next_cold_resize_sequence(current).map(Some)
            });
            assert!(result.is_err());
            assert_eq!(
                attempts, 1,
                "reserved saturation value is not retryable contention"
            );
        }
    }

    #[test]
    fn cold_resize_terminal_contention_waits_without_holding_other_guards() {
        let terminal = Arc::new(Mutex::new(guardian_lifetime_test_terminal()));
        let held = terminal.lock();
        let target = Arc::clone(&terminal);
        let (busy_tx, busy_rx) = sync_channel(1);
        let mut busy_tx = Some(busy_tx);
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let worker = std::thread::spawn(move || {
            retry_cold_resize_step(
                "terminal",
                &|| worker_cancelled.load(Ordering::Acquire),
                || {
                    let Some(mut term) = target.try_lock() else {
                        if let Some(tx) = busy_tx.take() {
                            tx.send(()).unwrap();
                        }
                        return Ok(None);
                    };
                    term.advance_bytes(b"released");
                    Ok(Some(term.cursor_pos().x))
                },
            )
        });
        let observed = busy_rx.recv_timeout(Duration::from_secs(5));
        drop(held);
        if observed.is_err() {
            cancelled.store(true, Ordering::Release);
        }
        let result = worker.join().unwrap().unwrap();
        observed.unwrap();
        assert_eq!(result, Some(8));
    }

    #[test]
    fn auxiliary_reset_returns_while_pane_output_is_blocked() {
        let pane = Arc::new(LocalPane::new(
            701,
            guardian_lifetime_test_terminal(),
            Box::new(KillCountingChild {
                kills: Arc::new(AtomicUsize::new(0)),
            }),
            Box::new(GuardianLifetimeTestMasterPty),
            Box::new(Vec::<u8>::new()),
            1,
            [0x71; 16],
            "auxiliary-control-test".to_string(),
        ));
        let registered: Arc<dyn Pane> = pane.clone();
        let mux = Arc::new(crate::Mux::new(None));
        let generation = crate::PaneRegistrationGeneration::new(
            pane.pane_id(),
            &mux.pane_retirements,
            Arc::downgrade(&mux),
        );
        {
            let _registration = mux.pane_registration.lock();
            mux.insert_pane_registration_locked(
                pane.pane_id(),
                pane.domain_id(),
                &registered,
                &generation,
            )
            .unwrap();
        }
        pane.terminal
            .lock()
            .perform_actions(vec![Action::Print('x')]);
        let blocked_output = pane.output_application.lock();
        let registration = mux.capture_pane_registration(&registered).unwrap();
        let (tx, rx) = sync_channel(1);
        let caller = std::thread::spawn(move || {
            tx.send(schedule_control_action(
                registration,
                PaneControlAction::Reset,
            ))
            .unwrap();
        });
        let admitted = rx.recv_timeout(Duration::from_millis(500));
        assert_eq!(pane.terminal.lock().cursor_pos().x, 1);
        drop(blocked_output);
        caller.join().unwrap();
        admitted
            .expect("GUI control must return before output can resume")
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while pane.terminal.lock().cursor_pos().x != 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            pane.terminal.lock().cursor_pos().x,
            0,
            "reset must eventually apply"
        );
    }

    #[test]
    fn auxiliary_rejection_does_not_construct_owned_output() {
        static WORKERS: AtomicUsize = AtomicUsize::new(0);
        let mut constructed = false;
        assert!(!spawn_auxiliary_output(
            &WORKERS,
            MAX_GENERATED_OUTPUT_MESSAGE_BYTES + 1,
            || {
                constructed = true;
                || {}
            }
        ));
        assert!(!constructed);
        assert_eq!(WORKERS.load(Ordering::Acquire), 0);
    }

    #[test]
    fn generated_output_returns_before_its_terminal_can_be_locked() {
        static WORKERS: AtomicUsize = AtomicUsize::new(0);
        let terminal = Arc::new(Mutex::new(guardian_lifetime_test_terminal()));
        let held = terminal.lock();
        let target = Arc::clone(&terminal);
        let (admitted_tx, admitted_rx) = sync_channel(1);
        let (done_tx, done_rx) = sync_channel(1);
        let caller = std::thread::spawn(move || {
            admitted_tx
                .send(spawn_generated_output(&WORKERS, "notice", move |actions| {
                    target.lock().perform_actions(actions);
                    done_tx.send(()).unwrap();
                }))
                .unwrap();
        });
        let admitted = admitted_rx.recv_timeout(Duration::from_millis(500));
        let premature = done_rx.recv_timeout(Duration::from_millis(50)).is_ok();
        drop(held);
        caller.join().unwrap();
        done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            admitted.unwrap(),
            "generated output must not run inline under a caller's lock"
        );
        assert!(!premature);
        assert_eq!(terminal.lock().cursor_pos().x, 6);
    }

    #[test]
    fn generated_output_worker_and_message_admission_are_bounded() {
        static WORKERS: AtomicUsize = AtomicUsize::new(0);
        let release = Arc::new((Mutex::new(false), parking_lot::Condvar::new()));
        let mut admitted = 0;
        for _ in 0..MAX_GENERATED_OUTPUT_WORKERS {
            let release = Arc::clone(&release);
            admitted += usize::from(spawn_generated_output(&WORKERS, "bounded", move |_| {
                let mut ready = release.0.lock();
                while !*ready {
                    release.1.wait(&mut ready);
                }
            }));
        }
        let refused =
            !spawn_generated_output(&WORKERS, "overflow", |_| panic!("overflow callback ran"));
        *release.0.lock() = true;
        release.1.notify_all();
        let deadline = Instant::now() + Duration::from_secs(5);
        while WORKERS.load(Ordering::Acquire) != 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(refused);
        assert_eq!(admitted, MAX_GENERATED_OUTPUT_WORKERS);
        assert_eq!(WORKERS.load(Ordering::Acquire), 0);
        assert!(!spawn_generated_output(
            &WORKERS,
            &"x".repeat(MAX_GENERATED_OUTPUT_MESSAGE_BYTES + 1),
            |_| panic!("oversized callback ran")
        ));
        assert_eq!(WORKERS.load(Ordering::Acquire), 0);
        assert!(spawn_generated_output(&WORKERS, "panic", |_| panic!(
            "synthetic generated-output callback panic"
        )));
        let deadline = Instant::now() + Duration::from_secs(5);
        while WORKERS.load(Ordering::Acquire) != 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            WORKERS.load(Ordering::Acquire),
            0,
            "panic must release admission"
        );
    }

    fn term_size(cols: usize, rows: usize) -> TerminalSize {
        TerminalSize {
            cols,
            rows,
            pixel_width: cols,
            pixel_height: rows,
            dpi: 96,
        }
    }

    fn pty_size(cols: u16, rows: u16) -> PtySize {
        PtySize {
            cols,
            rows,
            pixel_width: cols,
            pixel_height: rows,
        }
    }

    #[derive(Debug)]
    struct GuardianLifetimeTestTermConfig;

    impl TerminalConfiguration for GuardianLifetimeTestTermConfig {
        fn color_palette(&self) -> ColorPalette {
            ColorPalette::default()
        }
    }

    struct GuardianLifetimeTestMasterPty;

    impl MasterPty for GuardianLifetimeTestMasterPty {
        fn resize(&self, _size: PtySize) -> Result<(), Error> {
            Ok(())
        }

        fn get_size(&self) -> Result<PtySize, Error> {
            Ok(PtySize::default())
        }

        fn try_clone_reader(&self) -> Result<Box<dyn std::io::Read + Send>, Error> {
            Ok(Box::new(std::io::Cursor::new(Vec::new())))
        }

        fn take_writer(&self) -> Result<Box<dyn std::io::Write + Send>, Error> {
            Ok(Box::new(Vec::<u8>::new()))
        }

        #[cfg(unix)]
        fn process_group_leader(&self) -> Option<libc::pid_t> {
            None
        }

        #[cfg(unix)]
        fn as_raw_fd(&self) -> Option<std::os::fd::RawFd> {
            None
        }

        #[cfg(unix)]
        fn tty_name(&self) -> Option<std::path::PathBuf> {
            None
        }
    }

    struct GuardianLifetimeTestOutputReader;

    impl GuardianLiveOutputReader for GuardianLifetimeTestOutputReader {
        fn deliver_next_record(
            &mut self,
            _deliver: &mut dyn FnMut(
                crate::guardian_output_journal::GuardianOutputSegmentIdentity,
                crate::guardian_output_journal::GuardianOutputAppendReceipt,
                Arc<[u8]>,
            ) -> std::io::Result<()>,
        ) -> std::io::Result<GuardianLiveOutputDelivery> {
            Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "guardian lifetime fixture has no output",
            ))
        }
    }

    struct GuardianLifetimeTestCheckpointPublisher;

    impl GuardianLiveCheckpointPublisher for GuardianLifetimeTestCheckpointPublisher {
        fn publish_checkpoint(
            &self,
            _capture: LiveParserCheckpointAck,
        ) -> anyhow::Result<crate::guardian_checkpoint::PublishedGuardianCheckpoint> {
            anyhow::bail!("guardian lifetime fixture does not publish checkpoints")
        }
    }

    #[derive(Clone, Debug)]
    struct KillCountingChild {
        kills: Arc<AtomicUsize>,
    }

    impl ChildKiller for KillCountingChild {
        fn kill(&mut self) -> IoResult<()> {
            self.kills.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
            Box::new(self.clone())
        }
    }

    impl Child for KillCountingChild {
        fn try_wait(&mut self) -> IoResult<Option<ExitStatus>> {
            Ok(Some(ExitStatus::with_exit_code(0)))
        }

        fn wait(&mut self) -> IoResult<ExitStatus> {
            Ok(ExitStatus::with_exit_code(0))
        }

        fn process_id(&self) -> Option<u32> {
            None
        }
    }

    struct FencedGuardianLeaseControl {
        current: Mutex<GuardianPaneLeaseIdentity>,
        close_attempts: AtomicUsize,
        close_effects: AtomicUsize,
        retirement_attempts: AtomicUsize,
        retirement_effects: AtomicUsize,
    }

    impl FencedGuardianLeaseControl {
        fn new(current: GuardianPaneLeaseIdentity) -> Self {
            Self {
                current: Mutex::new(current),
                close_attempts: AtomicUsize::new(0),
                close_effects: AtomicUsize::new(0),
                retirement_attempts: AtomicUsize::new(0),
                retirement_effects: AtomicUsize::new(0),
            }
        }
    }

    impl GuardianPaneLeaseControl for FencedGuardianLeaseControl {
        fn close(&self, identity: GuardianPaneLeaseIdentity) -> Result<(), Error> {
            self.close_attempts.fetch_add(1, Ordering::SeqCst);
            if identity != *self.current.lock() {
                anyhow::bail!("stale guardian close lease");
            }
            self.close_effects.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn retire(&self, identity: GuardianPaneLeaseIdentity) -> Result<(), Error> {
            self.retirement_attempts.fetch_add(1, Ordering::SeqCst);
            if identity != *self.current.lock() {
                anyhow::bail!("stale guardian retirement lease");
            }
            self.retirement_effects.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn guardian_lifetime_test_terminal() -> Terminal {
        Terminal::new(
            term_size(80, 24),
            Arc::new(GuardianLifetimeTestTermConfig),
            "FrankenTerm",
            "guardian-lifetime-test",
            Box::new(Vec::new()),
        )
    }

    fn guardian_lifetime_test_identity(generation: u64) -> GuardianPaneLeaseIdentity {
        GuardianPaneLeaseIdentity::new(
            Uuid::from_bytes([0x11; 16]),
            Uuid::from_bytes([0x22; 16]),
            Uuid::from_bytes([0x33; 16]),
            generation,
        )
        .expect("nonzero guardian lease fixture")
    }

    fn guardian_lifetime_test_pane(
        pane_id: PaneId,
        identity: GuardianPaneLeaseIdentity,
        control: Arc<dyn GuardianPaneLeaseControl>,
        kills: Arc<AtomicUsize>,
    ) -> LocalPane {
        LocalPane::new_guardian_proxy(
            pane_id,
            guardian_lifetime_test_terminal(),
            Box::new(KillCountingChild { kills }),
            Box::new(GuardianLifetimeTestMasterPty),
            Box::new(Vec::<u8>::new()),
            1,
            identity,
            control,
            "guardian-lifetime-test".to_string(),
            Box::new(GuardianLifetimeTestOutputReader),
            Arc::new(GuardianLifetimeTestCheckpointPublisher),
            None,
            None,
        )
    }

    #[test]
    fn current_model_and_guardian_semantic_generation_are_strictly_disjoint() {
        let identity = guardian_lifetime_test_identity(1);
        let control = Arc::new(FencedGuardianLeaseControl::new(identity));
        let kills = Arc::new(AtomicUsize::new(0));
        let guardian_pane =
            guardian_lifetime_test_pane(888, identity, control.clone(), Arc::clone(&kills));
        assert_eq!(guardian_pane.current_model_semantic_generation(), None);
        let initial_seq = guardian_pane.current_guardian_semantic_generation();
        assert!(initial_seq.is_some());
        assert_eq!(guardian_pane.guardian_lease_identity(), Some(identity));

        // Same-byte resize mutation increments semantic generation without external bytes
        guardian_pane.terminal.lock().resize(TerminalSize {
            rows: 25,
            cols: 81,
            pixel_width: 0,
            pixel_height: 0,
            dpi: 0,
        });
        let resized_seq = guardian_pane.current_guardian_semantic_generation();
        assert!(resized_seq > initial_seq);

        let model_pane = LocalPane::new(
            889,
            guardian_lifetime_test_terminal(),
            Box::new(KillCountingChild { kills }),
            Box::new(GuardianLifetimeTestMasterPty),
            Box::new(Vec::<u8>::new()),
            1,
            [0x88; 16],
            "model-pane-test".to_string(),
        );
        assert!(model_pane.current_model_semantic_generation().is_some());
        assert_eq!(model_pane.current_guardian_semantic_generation(), None);
        assert_eq!(model_pane.guardian_lease_identity(), None);
    }

    #[test]
    fn title_metadata_retains_coherent_state_without_waiting_for_reflow() {
        let mut terminal = guardian_lifetime_test_terminal();
        terminal.advance_bytes(b"\x1b]2;before\x07\x1b]9;4;1;25\x07");
        let pane = LocalPane::new(
            700,
            terminal,
            Box::new(KillCountingChild {
                kills: Arc::new(AtomicUsize::new(0)),
            }),
            Box::new(GuardianLifetimeTestMasterPty),
            Box::new(Vec::<u8>::new()),
            1,
            [0x70; 16],
            "native-title-snapshot-test".to_string(),
        );
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let pane_ref = &pane;
            let holder = scope.spawn(move || {
                let mut terminal = pane_ref.terminal.lock();
                terminal.advance_bytes(b"\x1b]2;after\x07\x1b]9;4;1;75\x07");
                locked_tx.send(()).unwrap();
                // Bound a regression failure without leaving a deadlocked test.
                release_rx.recv_timeout(Duration::from_secs(10)).is_ok()
            });
            locked_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            let metadata = pane.get_title_metadata();
            let _ = release_tx.send(());
            assert!(holder.join().unwrap(), "title read waited for reflow");
            assert_eq!(metadata.title, "before");
            assert_eq!(metadata.progress, Progress::Percentage(25));
            assert!(metadata.is_stale);
        });
        let metadata = pane.get_title_metadata();
        assert_eq!(metadata.title, "after");
        assert_eq!(metadata.progress, Progress::Percentage(75));
        assert!(!metadata.is_stale);
        assert!(pane.terminal.try_lock().is_some());
    }

    #[test]
    fn native_render_snapshot_releases_terminal_and_rejects_stale_appdata() {
        struct Render<'a> {
            pane: &'a LocalPane,
            metadata: Arc<u32>,
            change_source: bool,
            decorate: bool,
            clear_scrollback: bool,
            called: bool,
        }
        impl WithPaneLines for Render<'_> {
            fn with_lines_mut(&mut self, first: StableRowIndex, lines: &mut [&mut Line]) {
                assert_eq!(first, 0);
                assert_eq!(lines.len(), 1);
                let mut term =
                    self.pane.terminal.try_lock().expect(
                        "native rendering must release the terminal mutex before its callback",
                    );
                if self.change_source {
                    term.advance_bytes(b"\rchanged");
                }
                if self.clear_scrollback {
                    term.screen_mut().erase_scrollback().unwrap();
                }
                if self.decorate {
                    let seqno = lines[0].current_seqno();
                    *lines[0] = Line::from_text("overlay", &Default::default(), seqno, None);
                }
                lines[0].set_appdata(Arc::clone(&self.metadata));
                self.called = true;
            }
        }

        for (change_source, decorate, clear_scrollback) in [
            (false, false, false),
            (true, false, false),
            (false, true, false),
            (false, false, true),
        ] {
            let pane = LocalPane::new(
                700,
                guardian_lifetime_test_terminal(),
                Box::new(KillCountingChild {
                    kills: Arc::new(AtomicUsize::new(0)),
                }),
                Box::new(GuardianLifetimeTestMasterPty),
                Box::new(Vec::<u8>::new()),
                1,
                [0x70; 16],
                "native-render-snapshot-test".to_string(),
            );
            #[cfg(not(feature = "disruptor-pane-io"))]
            pane.terminal.lock().advance_bytes(b"original");
            #[cfg(feature = "disruptor-pane-io")]
            {
                let mut parser = termwiz::escape::parser::Parser::new();
                let mut staged = Vec::new();
                parser.parse(b"original", |action| action.append_to(&mut staged));
                // Force the producer to stage output. Snapshot capture must
                // apply it before cloning, without reacquiring the mutex.
                let _terminal = pane.terminal.lock();
                pane.perform_actions(staged);
                assert!(!pane.action_ring.is_empty());
            }
            let mut render = Render {
                pane: &pane,
                metadata: Arc::new(42),
                change_source,
                decorate,
                clear_scrollback,
                called: false,
            };
            pane.with_lines_mut_and_apply_hyperlinks(0..1, &[], &mut render);
            assert!(render.called);
            #[cfg(feature = "disruptor-pane-io")]
            assert!(pane.action_ring.is_empty());
            let (_, lines) = pane.get_lines(0..1);
            assert_eq!(lines.len(), 1);
            assert_eq!(
                lines[0].get_appdata().is_some(),
                !change_source && !decorate && !clear_scrollback,
                "only unchanged source and rendered content may retain shape metadata",
            );
            assert!(lines[0].as_str().starts_with(if change_source {
                "changed"
            } else {
                "original"
            }));
        }
    }

    #[test]
    fn native_render_snapshot_does_not_wait_for_busy_cache_writeback() {
        struct Render {
            start: std::sync::mpsc::Sender<()>,
            locked: std::sync::mpsc::Receiver<()>,
            metadata: Arc<u32>,
        }
        impl WithPaneLines for Render {
            fn with_lines_mut(&mut self, _: StableRowIndex, lines: &mut [&mut Line]) {
                assert_eq!(lines.len(), 1);
                lines[0].set_appdata(Arc::clone(&self.metadata));
                self.start.send(()).unwrap();
                self.locked.recv_timeout(Duration::from_secs(10)).unwrap();
            }
        }
        let pane = LocalPane::new(
            700,
            guardian_lifetime_test_terminal(),
            Box::new(KillCountingChild {
                kills: Arc::new(AtomicUsize::new(0)),
            }),
            Box::new(GuardianLifetimeTestMasterPty),
            Box::new(Vec::<u8>::new()),
            1,
            [0x70; 16],
            "native-render-contention-test".to_string(),
        );
        pane.terminal.lock().advance_bytes(b"original");
        let (start_tx, start_rx) = std::sync::mpsc::channel();
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let mut render = Render {
            start: start_tx,
            locked: locked_rx,
            metadata: Arc::new(42),
        };
        std::thread::scope(|scope| {
            let pane_ref = &pane;
            let holder = scope.spawn(move || {
                start_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                let guard = pane_ref.terminal.lock();
                locked_tx.send(()).unwrap();
                // The deadline bounds a regression failure: a blocking
                // writeback can finish only after this fallback releases it.
                let released_by_render = release_rx.recv_timeout(Duration::from_secs(10)).is_ok();
                drop(guard);
                released_by_render
            });
            pane.with_lines_mut_and_apply_hyperlinks(0..1, &[], &mut render);
            let _ = release_tx.send(());
            assert!(
                holder.join().unwrap(),
                "render waited for optional cache writeback"
            );
        });
        let (_, lines) = pane.get_lines(0..1);
        assert!(lines[0].get_appdata().is_none());
        assert!(lines[0].as_str().starts_with("original"));
    }

    #[test]
    fn native_owned_read_publication_keeps_compatible_cold_prefix_without_sequence_churn() {
        let sink = Arc::new(ColdResizeTestSink::default());
        sink.witness_admission.store(true, Ordering::Relaxed);
        let mut terminal = Terminal::new(
            term_size(20, 2),
            Arc::new(ColdResizeTestConfig(sink.clone())),
            "FrankenTerm",
            "cold-prefix-publication-test",
            Box::new(Vec::new()),
        );
        terminal.advance_bytes(b"first\r\nsecond\r\nthird\r\nfourth\r\nfifth\r\n");
        let pane = LocalPane::new(
            720,
            terminal,
            Box::new(KillCountingChild {
                kills: Arc::new(AtomicUsize::new(0)),
            }),
            Box::new(GuardianLifetimeTestMasterPty),
            Box::new(Vec::<u8>::new()),
            1,
            [0x80; 16],
            "cold-prefix-publication-test".to_string(),
        );
        let (first, prior_end) = {
            let rows = sink.rows.lock();
            (
                *rows.1.first_key_value().expect("output spilled history").0,
                *rows.1.last_key_value().unwrap().0 + 1,
            )
        };
        let prior = pane
            .capture_line_read(first..first + 1, &mut Default::default())
            .unwrap()
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        assert_eq!(
            prior.lines().next().unwrap().as_str().trim_end_matches(' '),
            "first"
        );
        {
            let _busy = sink.rows.lock();
            assert_eq!(
                pane.publish_line_reads(std::slice::from_ref(&prior), &mut || {
                    panic!("busy cold source was published")
                }),
                Err(frankenterm_term::screen::ColdReadMetadataBusy),
            );
        }
        assert!(pane
            .publish_line_reads(std::slice::from_ref(&prior), &mut || {})
            .unwrap());
        assert!(pane.get_line_layout().unwrap().is_some());

        pane.terminal
            .lock()
            .advance_bytes(b"sixth\r\nseventh\r\neighth\r\n");
        assert!(*sink.rows.lock().1.last_key_value().unwrap().0 >= prior_end);
        let successor = pane
            .capture_line_read(prior_end..prior_end + 1, &mut Default::default())
            .unwrap()
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        assert!(pane
            .publish_line_reads(std::slice::from_ref(&successor), &mut || {})
            .unwrap());
        assert!(pane.terminal.lock().screen().validates_line_read(&prior));
        let (floor, dimensions) = pane.get_line_layout().unwrap().unwrap();
        let sequence = pane.get_current_seqno();
        let mut publications = 0;
        assert!(
            pane.publish_line_reads_at_layout(
                std::slice::from_ref(&prior),
                sequence,
                dimensions,
                &mut || publications += 1,
            )
            .unwrap(),
            "retained prefix must publish without restarting a same-layout read"
        );
        assert_eq!(publications, 1);
        assert_eq!(pane.get_current_seqno(), sequence);
        assert_eq!(pane.get_line_layout().unwrap().unwrap().0, floor);

        // Geometric compatibility is not source authority. Replacing a row
        // at the same coordinate must reject the captured bytes independently.
        {
            let mut rows = sink.rows.lock();
            rows.0 = Default::default();
            rows.1.insert(
                first,
                Line::from_text("changed", &termwiz::cell::CellAttributes::blank(), 99, None),
            );
        }
        assert!(!pane
            .publish_line_reads(std::slice::from_ref(&prior), &mut || {
                panic!("replaced cold source was published")
            })
            .unwrap());
    }

    #[test]
    fn native_owned_read_publication_is_nonblocking_and_rejects_mutation() {
        let pane = LocalPane::new(
            700,
            guardian_lifetime_test_terminal(),
            Box::new(KillCountingChild {
                kills: Arc::new(AtomicUsize::new(0)),
            }),
            Box::new(GuardianLifetimeTestMasterPty),
            Box::new(Vec::<u8>::new()),
            1,
            [0x70; 16],
            "native-owned-read".to_string(),
        );
        pane.terminal.lock().advance_bytes(b"original");
        let read = pane
            .capture_line_read(0..1, &mut Default::default())
            .unwrap()
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        let mut publications = 0;
        let (layout_seqno, layout_dimensions) = pane.get_line_layout().unwrap().unwrap();
        assert!(pane
            .publish_line_reads_at_layout(
                std::slice::from_ref(&read),
                layout_seqno,
                layout_dimensions,
                &mut || {}
            )
            .unwrap());
        let mut wrong_dimensions = layout_dimensions;
        wrong_dimensions.cols += 1;
        assert!(!pane
            .publish_line_reads_at_layout(
                std::slice::from_ref(&read),
                layout_seqno,
                wrong_dimensions,
                &mut || panic!("wrong layout published")
            )
            .unwrap());
        assert!(!pane
            .publish_line_reads_at_layout(
                std::slice::from_ref(&read),
                SequenceNo::MAX,
                layout_dimensions,
                &mut || panic!("saturated authority published")
            )
            .unwrap());
        assert!(pane
            .publish_line_reads(std::slice::from_ref(&read), &mut || {
                assert!(
                    pane.terminal.try_lock().is_none(),
                    "validation and publication share the terminal fence"
                );
                publications += 1;
            })
            .unwrap());
        {
            let _busy = pane.terminal.lock();
            assert_eq!(
                pane.get_line_layout(),
                Err(frankenterm_term::screen::ColdReadMetadataBusy)
            );
            assert_eq!(
                pane.publish_line_reads_at_layout(
                    std::slice::from_ref(&read),
                    layout_seqno,
                    layout_dimensions,
                    &mut || panic!("busy terminal published"),
                ),
                Err(frankenterm_term::screen::ColdReadMetadataBusy),
            );
            assert!(pane.selection_source_snapshot().is_none());
            assert_eq!(
                pane.publish_line_reads(std::slice::from_ref(&read), &mut || publications += 1),
                Err(frankenterm_term::screen::ColdReadMetadataBusy)
            );
            assert!(pane
                .capture_line_read(0..1, &mut Default::default())
                .unwrap()
                .is_err());
        }
        let before_busy = pane.get_current_seqno();
        {
            let _busy = pane.line_layout_observation.lock();
            assert_eq!(
                pane.get_line_layout(),
                Err(frankenterm_term::screen::ColdReadMetadataBusy)
            );
            assert_eq!(
                pane.publish_line_reads_at_layout(
                    std::slice::from_ref(&read),
                    layout_seqno,
                    layout_dimensions,
                    &mut || panic!("busy layout observation published"),
                ),
                Err(frankenterm_term::screen::ColdReadMetadataBusy),
            );
        }
        assert_eq!(pane.get_current_seqno(), before_busy);
        assert!(
            pane.publish_line_reads_at_layout(
                std::slice::from_ref(&read),
                layout_seqno,
                layout_dimensions,
                &mut || {},
            )
            .unwrap(),
            "released contention must permit the exact unchanged source"
        );
        pane.terminal.lock().advance_bytes(b" changed");
        assert!(!pane
            .publish_line_reads_at_layout(
                std::slice::from_ref(&read),
                layout_seqno,
                layout_dimensions,
                &mut || panic!("stale layout published")
            )
            .unwrap());
        assert!(!pane
            .publish_line_reads(std::slice::from_ref(&read), &mut || publications += 1)
            .unwrap());
        assert_eq!(publications, 1);
        // Continued output on another row must not starve an exact retained
        // row read. The terminal content sequence advances, layout floor does
        // not, and the old read still passes its independent source check.
        // CR marks the previous cursor row dirty even without changing text;
        // move off the requested row before capturing the exact source.
        pane.terminal.lock().advance_bytes(b"\r\n");
        let fresh = pane
            .capture_line_read(0..1, &mut Default::default())
            .unwrap()
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        let (floor, dimensions) = pane.get_line_layout().unwrap().unwrap();
        let observed = pane.get_current_seqno();
        let source_seqno = fresh.lines().next().unwrap().current_seqno();
        pane.terminal.lock().advance_bytes(b"other row");
        assert_eq!(pane.get_lines(0..1).1[0].current_seqno(), source_seqno);
        assert!(pane.get_current_seqno() > observed);
        assert_eq!(pane.get_line_layout().unwrap().unwrap().0, floor);
        assert!(pane
            .publish_line_reads_at_layout(
                std::slice::from_ref(&fresh),
                observed,
                dimensions,
                &mut || {}
            )
            .unwrap());
        pane.terminal
            .lock()
            .advance_bytes(b"\x1b[?1049h\x1b[?1049l");
        assert!(
            pane.get_line_layout().unwrap().unwrap().0 > observed,
            "unobserved alternate-screen round trip cannot reuse layout authority"
        );
        assert!(!pane
            .publish_line_reads_at_layout(
                std::slice::from_ref(&fresh),
                observed,
                dimensions,
                &mut || panic!("alternate-screen ABA published")
            )
            .unwrap());
    }

    #[test]
    fn native_render_frame_is_owned_and_captures_both_damage_baselines() {
        let pane = LocalPane::new(
            701,
            guardian_lifetime_test_terminal(),
            Box::new(KillCountingChild {
                kills: Arc::new(AtomicUsize::new(0)),
            }),
            Box::new(GuardianLifetimeTestMasterPty),
            Box::new(Vec::<u8>::new()),
            1,
            [0x71; 16],
            "native-render-frame".to_string(),
        );
        pane.terminal.lock().advance_bytes(b"original");
        let source = pane.get_current_seqno();
        let frame = pane
            .try_capture_render_frame(None, 0, source, &[], false)
            .unwrap();
        assert_eq!(frame.source_sequence, source);
        assert_eq!(frame.dimensions, pane.get_dimensions());
        assert_eq!(frame.cursor, pane.get_cursor_position());
        assert!(frame.dirty.contains(frame.first));
        assert!(frame.selection_dirty.is_empty());
        assert!(frame.lines[0].as_str().starts_with("original"));
        pane.terminal.lock().advance_bytes(b" changed");
        pane.publish_render_frame_appdata(&frame);
        assert!(!frame.lines[0].as_str().contains("changed"));
        assert!(pane.get_lines(frame.first..frame.first + 1).1[0]
            .as_str()
            .contains("changed"));
        let dimensions = pane.get_dimensions();
        for requested in [StableRowIndex::MIN, StableRowIndex::MAX] {
            let normalized = pane
                .try_capture_render_frame(Some(requested), 0, 0, &[], false)
                .expect("an out-of-range viewport must converge to available rows");
            assert_eq!(
                normalized.first,
                requested
                    .max(dimensions.scrollback_top)
                    .min(dimensions.physical_top)
            );
            assert_eq!(normalized.lines.len(), dimensions.viewport_rows);
        }
    }

    #[test]
    fn native_render_frame_hyperlinks_settle_one_source_before_selection() {
        use frankenterm_term::screen::SelectionAnchorCoordinate;

        let pane = LocalPane::new(
            705,
            guardian_lifetime_test_terminal(),
            Box::new(KillCountingChild {
                kills: Arc::new(AtomicUsize::new(0)),
            }),
            Box::new(GuardianLifetimeTestMasterPty),
            Box::new(Vec::<u8>::new()),
            1,
            [0x75; 16],
            "native-hyperlink-frame".to_string(),
        );
        pane.terminal.lock().advance_bytes(b"https://example.com");
        let (floor, before, dimensions) = pane.selection_source_snapshot().unwrap();
        let rules = [termwiz::hyperlink::Rule::new(r"https://[a-z.]+", "$0").unwrap()];
        let frame = pane
            .try_capture_render_frame(None, before, before, &rules, false)
            .unwrap();
        assert!(frame.source_sequence > before);
        assert_eq!(frame.layout_floor, floor);
        assert!(frame.dirty.contains(frame.first));
        assert!(frame.selection_dirty.contains(frame.first));
        assert!(frame.lines[0]
            .visible_cells()
            .any(|cell| cell.attrs().hyperlink().is_some()));
        assert_eq!(
            pane.selection_source_snapshot().unwrap(),
            (floor, frame.source_sequence, dimensions)
        );

        let points = [Some(SelectionAnchorCoordinate {
            row: frame.first,
            column: Some(0),
        }); 3];
        assert!(pane
            .capture_selection_anchor(floor, before, dimensions, points)
            .is_err());
        let anchor = pane
            .capture_selection_anchor(floor, frame.source_sequence, dimensions, points)
            .unwrap()
            .unwrap();
        // Line retains a weak cache reference; the renderer owns the payload.
        let metadata = Arc::new(42u32);
        frame.lines[0].set_appdata(Arc::clone(&metadata));
        pane.publish_render_frame_appdata(&frame);

        // A real first-scan edit advances the source once. Repeated painting
        // and renderer-cache publication must neither dirty the frame nor
        // invalidate selection authority for this exact displayed source.
        for _ in 0..3 {
            let next = pane
                .try_capture_render_frame(
                    None,
                    frame.source_sequence,
                    frame.source_sequence,
                    &rules,
                    false,
                )
                .unwrap();
            assert_eq!(next.source_sequence, frame.source_sequence);
            assert!(next.dirty.is_empty());
            assert!(next.selection_dirty.is_empty());
            assert_eq!(next.lines, frame.lines);
            assert!(next.lines[0].get_appdata().is_some());
            assert_eq!(
                pane.selection_anchor_snapshot(&anchor).unwrap(),
                (floor, frame.source_sequence, dimensions, Some(points))
            );
        }
    }

    #[test]
    fn native_selection_anchor_follows_resize_publication_and_rejects_screen_switch() {
        use frankenterm_term::screen::SelectionAnchorCoordinate;
        let pane = LocalPane::new(
            704,
            guardian_lifetime_test_terminal(),
            Box::new(KillCountingChild {
                kills: Arc::new(AtomicUsize::new(0)),
            }),
            Box::new(GuardianLifetimeTestMasterPty),
            Box::new(Vec::<u8>::new()),
            1,
            [0x74; 16],
            "native-selection-anchor".to_string(),
        );
        pane.terminal.lock().advance_bytes("A界BCDEF".as_bytes());
        let (floor, sequence, dimensions) = pane.selection_source_snapshot().unwrap();
        let points = [Some(SelectionAnchorCoordinate {
            row: 0,
            column: Some(3),
        }); 3];
        assert!(pane
            .capture_selection_anchor(floor, sequence.saturating_add(1), dimensions, points)
            .is_err());
        let mut wrong_dimensions = dimensions;
        wrong_dimensions.cols += 1;
        assert!(pane
            .capture_selection_anchor(floor, sequence, wrong_dimensions, points)
            .is_err());
        let token = pane
            .capture_selection_anchor(floor, sequence, dimensions, points)
            .unwrap()
            .unwrap();
        {
            let _busy = pane.terminal.lock();
            assert_eq!(
                pane.capture_selection_anchor(floor, sequence, dimensions, points),
                Err(SelectionAnchorCaptureError::Busy)
            );
            assert!(pane.selection_anchor_snapshot(&token).is_none());
        }
        assert_eq!(
            pane.selection_anchor_snapshot(&token).unwrap().3,
            Some(points)
        );
        for (cols, pixel_width, dpi) in [(2, 2, 96), (80, 80, 96), (80, 160, 96), (80, 160, 120)] {
            let mut size = term_size(cols, 24);
            size.pixel_width = pixel_width;
            size.dpi = dpi;
            let mut pty_size = pty_size(cols as u16, 24);
            pty_size.pixel_width = pixel_width as u16;
            let pending = {
                let mut queue = pane.resize_queue.lock();
                queue.enqueue(size, pty_size, Instant::now());
                queue.dequeue_for_worker().unwrap()
            };
            let metrics = LocalPane::apply_resize_sync(
                pane.pane_id,
                &pane.terminal,
                &pane.line_layout_observation,
                #[cfg(feature = "disruptor-pane-io")]
                &pane.action_ring,
                &pane.pty,
                &pane.resize_queue,
                pending.seq,
                size,
                pty_size,
                ResizeCancellationToken::new(pending.seq),
            )
            .unwrap();
            assert!(!metrics.cancelled && !metrics.noop);
            let frame = pane
                .try_capture_render_frame(None, 0, 0, &[], false)
                .unwrap();
            let (floor, sequence, dimensions, points) =
                pane.selection_anchor_snapshot(&token).unwrap();
            assert_eq!(floor, frame.layout_floor);
            assert_eq!(sequence, frame.source_sequence);
            assert_eq!(dimensions, frame.dimensions);
            assert_eq!(
                points,
                Some(
                    [Some(SelectionAnchorCoordinate {
                        row: if cols == 2 { 2 } else { 0 },
                        column: Some(if cols == 2 { 0 } else { 3 }),
                    }); 3]
                )
            );
        }
        pane.terminal
            .lock()
            .advance_bytes(b"\x1b[?1049h\x1b[?1049l");
        assert!(
            pane.selection_anchor_snapshot(&token).unwrap().3.is_none(),
            "an alternate-screen ABA cannot reauthorize the original token"
        );
        pane.terminal.lock().advance_bytes(b"\x1b[?1049h");
        let (floor, sequence, dimensions) = pane.selection_source_snapshot().unwrap();
        assert_eq!(
            pane.capture_selection_anchor(floor, sequence, dimensions, points),
            Ok(None),
            "current alternate-screen selection is valid but has no reflow anchor"
        );
        pane.terminal.lock().advance_bytes(b"\x1b[?1049l");
        let (floor, sequence, dimensions) = pane.selection_source_snapshot().unwrap();
        let nonresident = [Some(SelectionAnchorCoordinate {
            row: -1,
            column: Some(0),
        }); 3];
        assert_eq!(
            pane.capture_selection_anchor(floor, sequence, dimensions, nonresident),
            Ok(None),
            "nonresident history cannot produce a hot-row anchor"
        );
        drop(token);
        let retained: Vec<_> = (0..16)
            .map(|_| {
                pane.capture_selection_anchor(floor, sequence, dimensions, points)
                    .unwrap()
                    .expect("resident point must fill an available registry slot")
            })
            .collect();
        assert_eq!(
            pane.capture_selection_anchor(floor, sequence, dimensions, points),
            Ok(None),
            "registry capacity refuses remapping, not ordinary selection"
        );
        assert_eq!(retained.len(), 16);
    }

    #[test]
    fn native_selection_capture_accepts_only_proven_unchanged_selected_rows() {
        use frankenterm_term::screen::SelectionAnchorCoordinate;
        let pane = LocalPane::new(
            706,
            guardian_lifetime_test_terminal(),
            Box::new(KillCountingChild {
                kills: Arc::new(AtomicUsize::new(0)),
            }),
            Box::new(GuardianLifetimeTestMasterPty),
            Box::new(Vec::<u8>::new()),
            1,
            [0x76; 16],
            "selection-source-proof".to_string(),
        );
        pane.terminal.lock().advance_bytes(b"selected text");
        let (floor, sequence, dimensions) = pane.selection_source_snapshot().unwrap();
        let points = [Some(SelectionAnchorCoordinate {
            row: 0,
            column: Some(3),
        }); 3];
        {
            let _busy = pane.terminal.lock();
            assert_eq!(
                pane.capture_selection_anchor(floor, sequence, dimensions, points),
                Err(SelectionAnchorCaptureError::Busy)
            );
        }
        pane.terminal
            .lock()
            .advance_bytes(b"\x1b[2;1Hunrelated output");
        let token = pane
            .capture_selection_anchor(floor, sequence, dimensions, points)
            .expect("unrelated output must not invalidate unchanged selected rows")
            .expect("resident unchanged points must receive an anchor");
        assert_eq!(
            pane.selection_anchor_snapshot(&token).unwrap().3,
            Some(points)
        );
        pane.terminal
            .lock()
            .advance_bytes(b"\x1b[1;1Hchanged selection");
        assert_eq!(
            pane.capture_selection_anchor(floor, sequence, dimensions, points),
            Err(SelectionAnchorCaptureError::SourceChanged)
        );
    }

    #[test]
    fn native_resize_finalizes_source_before_first_frame_and_preserves_later_changes() {
        let pane = LocalPane::new(
            703,
            guardian_lifetime_test_terminal(),
            Box::new(KillCountingChild {
                kills: Arc::new(AtomicUsize::new(0)),
            }),
            Box::new(GuardianLifetimeTestMasterPty),
            Box::new(Vec::<u8>::new()),
            1,
            [0x73; 16],
            "native-resize-source".to_string(),
        );
        pane.terminal.lock().advance_bytes("original 界".as_bytes());
        let mut previous = pane
            .try_capture_render_frame(None, 0, 0, &[], false)
            .unwrap();
        for cols in [123, 80] {
            let size = term_size(cols, 24);
            let pty_size = pty_size(cols as u16, 24);
            let pending = {
                let mut queue = pane.resize_queue.lock();
                queue.enqueue(size, pty_size, Instant::now());
                queue.dequeue_for_worker().unwrap()
            };
            let metrics = LocalPane::apply_resize_sync(
                pane.pane_id,
                &pane.terminal,
                &pane.line_layout_observation,
                #[cfg(feature = "disruptor-pane-io")]
                &pane.action_ring,
                &pane.pty,
                &pane.resize_queue,
                pending.seq,
                size,
                pty_size,
                ResizeCancellationToken::new(pending.seq),
            )
            .unwrap();
            assert!(!metrics.cancelled && !metrics.noop);
            // Observe the worker's committed source before any reader can
            // perform the lazy floor refresh that caused the native failure.
            let committed_source = pane.terminal.lock().current_seqno();
            assert!(committed_source > previous.source_sequence);
            for _ in 0..3 {
                let frame = pane
                    .try_capture_render_frame(None, 0, 0, &[], false)
                    .unwrap();
                assert_eq!(frame.source_sequence, committed_source);
                assert_eq!(frame.layout_floor, committed_source);
                assert_eq!(frame.dimensions.cols, cols);
                assert!(frame.lines[0].as_str().starts_with("original 界"));
                assert_eq!(
                    pane.selection_source_snapshot().unwrap().1,
                    committed_source
                );
                previous = frame;
            }
        }

        // Genuine later content must remain distinguishable from the resize
        // receipt, even when its coordinates and layout floor did not change.
        let committed_source = previous.source_sequence;
        pane.terminal.lock().advance_bytes(b" changed");
        let later = pane
            .try_capture_render_frame(None, 0, 0, &[], false)
            .unwrap();
        assert!(later.source_sequence > committed_source);
        assert_eq!(later.layout_floor, previous.layout_floor);
        assert!(later.lines[0].as_str().contains("changed"));
        pane.terminal
            .lock()
            .advance_bytes(b"\x1b[?1049h\x1b[?1049l");
        let after_aba = pane
            .try_capture_render_frame(None, 0, 0, &[], false)
            .unwrap();
        assert!(after_aba.layout_floor > later.source_sequence);

        // Causal negative: bypass only the worker's observation publication.
        // The real terminal resize still changes geometry, and the first
        // reader must advance the unfinalized source rather than bless it.
        let unfinalized_source = {
            let mut term = pane.terminal.lock();
            term.resize(term_size(123, 24));
            term.current_seqno()
        };
        let first = pane
            .try_capture_render_frame(None, 0, 0, &[], false)
            .unwrap();
        assert!(first.source_sequence > unfinalized_source);
        assert_eq!(first.dimensions.cols, 123);
    }

    #[test]
    fn native_resize_source_publication_defers_busy_observation_in_every_phase() {
        for phase in ["primary", "cold_seam", "cold_index"] {
            let mut term = guardian_lifetime_test_terminal();
            let observation = Mutex::new(None);
            LocalPane::refresh_line_layout_floor(&observation, &mut term).unwrap();
            term.resize(term_size(123, 24));
            let before = term.current_seqno();
            let busy = observation.lock();
            assert!(!LocalPane::publish_resize_source(
                703,
                &observation,
                &mut term,
                ResizeCancellationToken::new(1),
                1,
                phase,
            ));
            assert_eq!(term.current_seqno(), before);
            drop(busy);
            assert!(LocalPane::publish_resize_source(
                703,
                &observation,
                &mut term,
                ResizeCancellationToken::new(1),
                1,
                phase,
            ));
            let published = term.current_seqno();
            assert!(published > before);
            assert_eq!(
                LocalPane::refresh_line_layout_floor(&observation, &mut term),
                Ok(Some(published))
            );
            assert_eq!(term.current_seqno(), published);
        }
    }

    #[test]
    fn native_resize_source_publication_rejects_sequence_saturation() {
        use frankenterm_term::terminalstate::checkpoint::TerminalCheckpointV2;

        // Restore an otherwise valid real model near the sequence boundary;
        // no test-only setter or replacement terminal implementation is needed.
        let limits = TerminalCheckpointLimits::default();
        let checkpoint = guardian_lifetime_test_terminal()
            .capture_recovery_checkpoint(limits)
            .unwrap();
        let mut payload: serde_json::Value =
            serde_json::from_slice(checkpoint.canonical_payload()).unwrap();
        payload["seqno"] = serde_json::json!(SequenceNo::MAX - 3);
        let checkpoint: TerminalCheckpointV2 = serde_json::from_value(payload).unwrap();
        let encoded = checkpoint.to_canonical_json(limits).unwrap();
        let inert = TerminalCheckpointV2::decode_canonical_json(&encoded, limits)
            .unwrap()
            .restore_inert(Arc::new(GuardianLifetimeTestTermConfig))
            .unwrap();
        assert_eq!(
            serde_json::to_value(inert.checkpoint().unwrap()).unwrap()["seqno"],
            serde_json::json!(SequenceNo::MAX - 3)
        );
        // Activation itself invalidates the replay source. Reserve that step
        // before exercising resize and the final publication boundary.
        let mut term = inert.into_live(Box::new(Vec::<u8>::new())).unwrap();
        assert_eq!(term.current_seqno(), SequenceNo::MAX - 2);
        let observation = Mutex::new(None);
        assert_eq!(
            LocalPane::refresh_line_layout_floor(&observation, &mut term),
            Ok(Some(SequenceNo::MAX - 2))
        );
        term.resize(term_size(123, 24));
        assert_eq!(term.current_seqno(), SequenceNo::MAX - 1);
        for phase in ["primary", "cold_seam", "cold_index"] {
            assert!(!LocalPane::publish_resize_source(
                703,
                &observation,
                &mut term,
                ResizeCancellationToken::new(1),
                1,
                phase,
            ));
            assert_eq!(term.current_seqno(), SequenceNo::MAX);
            assert_eq!(
                LocalPane::refresh_line_layout_floor(&observation, &mut term),
                Ok(None)
            );
        }
        let pane = make_legacy_test_pane(785, term);
        assert!(
            pane.capture_surface_snapshot(0).is_none(),
            "sequence exhaustion must reach the terminal source-fence rejection, not retryable busy"
        );
    }

    #[test]
    fn native_render_frame_does_not_wait_for_terminal_pty_tmux_or_layout_locks() {
        let pane = LocalPane::new(
            702,
            guardian_lifetime_test_terminal(),
            Box::new(KillCountingChild {
                kills: Arc::new(AtomicUsize::new(0)),
            }),
            Box::new(GuardianLifetimeTestMasterPty),
            Box::new(Vec::<u8>::new()),
            1,
            [0x72; 16],
            "native-render-busy".to_string(),
        );
        for lock in [0, 1, 2, 3] {
            if lock == 1 && !cfg!(unix) {
                continue;
            }
            let (locked_tx, locked_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            std::thread::scope(|scope| {
                let pane = &pane;
                let holder = scope.spawn(move || {
                    let wait = || {
                        locked_tx.send(()).unwrap();
                        release_rx.recv_timeout(Duration::from_secs(5)).is_ok()
                    };
                    match lock {
                        0 => {
                            let _guard = pane.terminal.lock();
                            wait()
                        }
                        1 => {
                            let _guard = pane.pty.lock();
                            wait()
                        }
                        2 => {
                            let _guard = pane.tmux_domain.lock();
                            wait()
                        }
                        _ => {
                            let _guard = pane.line_layout_observation.lock();
                            wait()
                        }
                    }
                });
                locked_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                let captured = pane.try_capture_render_frame(None, 0, 0, &[], true);
                let _ = release_tx.send(());
                assert!(holder.join().unwrap(), "capture waited for lock {}", lock);
                assert!(
                    captured.is_none(),
                    "busy lock {} must defer the frame",
                    lock
                );
            });
        }
    }

    #[test]
    fn cold_viewport_old_completion_cannot_clear_new_request() {
        let old = Arc::new(AtomicBool::new(false));
        let new = Arc::new(AtomicBool::new(false));
        let state = Arc::new(Mutex::new(Some(ColdViewportPending {
            requested: 20..30,
            cancelled: Arc::clone(&new),
        })));
        drop(ColdViewportCompletion {
            state: Arc::clone(&state),
            cancelled: old,
        });
        assert!(state
            .lock()
            .as_ref()
            .is_some_and(|pending| Arc::ptr_eq(&pending.cancelled, &new)));
        drop(ColdViewportCompletion {
            state: Arc::clone(&state),
            cancelled: new,
        });
        assert!(state.lock().is_none());
    }

    #[test]
    fn cold_viewport_retirement_transfers_last_owner_to_worker() {
        let pane = LocalPane::new(
            700,
            guardian_lifetime_test_terminal(),
            Box::new(KillCountingChild {
                kills: Arc::new(AtomicUsize::new(0)),
            }),
            Box::new(GuardianLifetimeTestMasterPty),
            Box::new(Vec::<u8>::new()),
            1,
            [0x70; 16],
            "native-read-retirement".to_string(),
        );
        let read = Arc::new(
            pane.capture_line_read(0..1, &mut Default::default())
                .unwrap()
                .unwrap()
                .hydrate(|| false)
                .unwrap(),
        );
        let weak = Arc::downgrade(&read);
        let (sender, receiver) = sync_channel(1);
        let retirement = ColdViewportRetirement {
            evicted: vec![ColdViewportEntry {
                registration: [1; 16],
                requested: 0..1,
                read: Arc::clone(&read),
            }],
            read: Some(read),
            sender,
        };
        drop(retirement);
        assert!(
            weak.upgrade().is_some(),
            "publication drop must not destroy queued payload"
        );
        std::thread::spawn(move || drop(receiver.recv_timeout(Duration::from_secs(5)).unwrap()))
            .join()
            .unwrap();
        assert!(
            weak.upgrade().is_none(),
            "worker retired both publication and evicted owners"
        );
    }

    #[test]
    fn guardian_lease_identity_rejects_reserved_zero_fences() {
        let valid = guardian_lifetime_test_identity(1);
        assert!(GuardianPaneLeaseIdentity::new(
            Uuid::nil(),
            valid.mux_incarnation(),
            valid.pane_id(),
            valid.generation(),
        )
        .is_err());
        assert!(GuardianPaneLeaseIdentity::new(
            valid.guardian_incarnation(),
            Uuid::nil(),
            valid.pane_id(),
            valid.generation(),
        )
        .is_err());
        assert!(GuardianPaneLeaseIdentity::new(
            valid.guardian_incarnation(),
            valid.mux_incarnation(),
            Uuid::nil(),
            valid.generation(),
        )
        .is_err());
        assert!(GuardianPaneLeaseIdentity::new(
            valid.guardian_incarnation(),
            valid.mux_incarnation(),
            valid.pane_id(),
            0,
        )
        .is_err());
    }

    #[test]
    fn legacy_local_pane_drop_retains_child_kill_contract() {
        let kills = Arc::new(AtomicUsize::new(0));
        let pane = LocalPane::new(
            700,
            guardian_lifetime_test_terminal(),
            Box::new(KillCountingChild {
                kills: Arc::clone(&kills),
            }),
            Box::new(GuardianLifetimeTestMasterPty),
            Box::new(Vec::<u8>::new()),
            1,
            [0x70; 16],
            "legacy-lifetime-test".to_string(),
        );

        drop(pane);

        assert_eq!(
            kills.load(Ordering::SeqCst),
            1,
            "removing the legacy Drop kill would leak its mux-owned child",
        );
    }

    #[test]
    fn guardian_local_pane_drop_retires_only_lease_and_never_kills_child() {
        let identity = guardian_lifetime_test_identity(1);
        let control = Arc::new(FencedGuardianLeaseControl::new(identity));
        let kills = Arc::new(AtomicUsize::new(0));
        let pane = guardian_lifetime_test_pane(701, identity, control.clone(), Arc::clone(&kills));
        assert_eq!(
            Pane::durable_pane_id(&pane),
            Some(*identity.pane_id().as_bytes()),
            "guardian proxy must derive durable identity from the fenced lease",
        );

        drop(pane);

        assert_eq!(control.retirement_attempts.load(Ordering::SeqCst), 1);
        assert_eq!(control.retirement_effects.load(Ordering::SeqCst), 1);
        assert_eq!(control.close_attempts.load(Ordering::SeqCst), 0);
        assert_eq!(
            kills.load(Ordering::SeqCst),
            0,
            "guardian ownership must make the legacy LocalPane Drop killer unreachable",
        );
    }

    #[test]
    fn unpublished_guardian_cancellation_retires_once_without_close_or_signal() {
        let identity = guardian_lifetime_test_identity(1);
        let control = Arc::new(FencedGuardianLeaseControl::new(identity));
        let kills = Arc::new(AtomicUsize::new(0));
        let pane = guardian_lifetime_test_pane(721, identity, control.clone(), Arc::clone(&kills));
        let unpublished = crate::domain::UnpublishedPane::from_guardian_proxy(pane)
            .expect("complete guardian facets admit unpublished ownership");
        let publication = unpublished.guardian_publication_receipt().unwrap();
        assert!(!publication.was_published());
        drop(unpublished);
        assert!(!publication.was_published());
        assert_eq!(control.retirement_attempts.load(Ordering::SeqCst), 1);
        assert_eq!(control.retirement_effects.load(Ordering::SeqCst), 1);
        assert_eq!(control.close_attempts.load(Ordering::SeqCst), 0);
        assert_eq!(kills.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn unpublished_guardian_admission_rejects_missing_or_consumed_facets() {
        for missing in 0..4 {
            let identity = guardian_lifetime_test_identity(1);
            let control = Arc::new(FencedGuardianLeaseControl::new(identity));
            let kills = Arc::new(AtomicUsize::new(0));
            let mut pane =
                guardian_lifetime_test_pane(722, identity, control.clone(), Arc::clone(&kills));
            match missing {
                0 => {
                    pane.guardian_live_output_reader.lock().take();
                }
                1 => {
                    pane.guardian_checkpoint_publisher.take();
                }
                2 => {
                    pane.durable_pane_id = [0x91; 16];
                }
                3 => {
                    Pane::kill(&pane);
                }
                _ => unreachable!(),
            }
            assert!(crate::domain::UnpublishedPane::from_guardian_proxy(pane).is_err());
            assert_eq!(kills.load(Ordering::SeqCst), 0);
            assert_eq!(
                control.close_attempts.load(Ordering::SeqCst),
                usize::from(missing == 3)
            );
            assert_eq!(
                control.retirement_attempts.load(Ordering::SeqCst),
                usize::from(missing != 3)
            );
        }
    }

    #[test]
    fn unpublished_guardian_admission_rejects_native_and_preserves_native_rollback() {
        let native = |kills| {
            LocalPane::new(
                723,
                guardian_lifetime_test_terminal(),
                Box::new(KillCountingChild { kills }),
                Box::new(GuardianLifetimeTestMasterPty),
                Box::new(Vec::<u8>::new()),
                1,
                [0x73; 16],
                "native-unpublished-test".to_string(),
            )
        };
        let kills = Arc::new(AtomicUsize::new(0));
        assert!(
            crate::domain::UnpublishedPane::from_guardian_proxy(native(Arc::clone(&kills)))
                .is_err()
        );
        assert_eq!(kills.load(Ordering::SeqCst), 1);

        let kills = Arc::new(AtomicUsize::new(0));
        let pane: Arc<dyn Pane> = Arc::new(native(Arc::clone(&kills)));
        let unpublished = crate::domain::UnpublishedPane::new(Arc::clone(&pane));
        assert!(unpublished.guardian_publication_receipt().is_none());
        drop(unpublished);
        assert_eq!(
            kills.load(Ordering::SeqCst),
            1,
            "native guard still explicitly kills before releasing its Arc"
        );
    }

    #[test]
    fn unpublished_guardian_publication_registers_then_rejects_readmission() {
        struct HeldReader(std::sync::mpsc::Receiver<()>);
        impl GuardianLiveOutputReader for HeldReader {
            fn deliver_next_record(
                &mut self,
                _deliver: &mut dyn FnMut(
                    crate::guardian_output_journal::GuardianOutputSegmentIdentity,
                    crate::guardian_output_journal::GuardianOutputAppendReceipt,
                    Arc<[u8]>,
                ) -> std::io::Result<()>,
            ) -> std::io::Result<GuardianLiveOutputDelivery> {
                let _ = self.0.recv_timeout(Duration::from_secs(5));
                Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "publication fixture released",
                ))
            }
        }
        let identity = guardian_lifetime_test_identity(1);
        let control = Arc::new(FencedGuardianLeaseControl::new(identity));
        let kills = Arc::new(AtomicUsize::new(0));
        let pane = guardian_lifetime_test_pane(724, identity, control.clone(), Arc::clone(&kills));
        let (release, wait) = std::sync::mpsc::channel();
        *pane.guardian_live_output_reader.lock() = Some(Box::new(HeldReader(wait)));
        let mux = Arc::new(crate::Mux::new(None));
        assert!(mux.get_pane(724).is_none());
        let unpublished = crate::domain::UnpublishedPane::from_guardian_proxy(pane).unwrap();
        let publication = unpublished.guardian_publication_receipt().unwrap();
        let publication_clone = publication.clone();
        assert!(!publication.was_published());
        let published = unpublished
            .publish(&mux)
            .expect("register guardian pane through consuming mux-owned publication");
        assert!(publication.was_published());
        assert!(publication_clone.was_published());
        assert!(mux.get_pane(724).is_some());
        let local = published
            .downcast_ref::<LocalPane>()
            .expect("published guardian LocalPane");
        assert!(
            local.validate_unpublished_guardian_proxy().is_err(),
            "registered pane cannot be admitted as unpublished"
        );
        assert_eq!(control.retirement_attempts.load(Ordering::SeqCst), 0);
        assert_eq!(control.close_attempts.load(Ordering::SeqCst), 0);
        assert_eq!(kills.load(Ordering::SeqCst), 0);
        let rejected_identity = GuardianPaneLeaseIdentity::new(
            identity.guardian_incarnation(),
            identity.mux_incarnation(),
            Uuid::from_bytes([0x44; 16]),
            1,
        )
        .unwrap();
        let rejected_control = Arc::new(FencedGuardianLeaseControl::new(rejected_identity));
        let rejected_kills = Arc::new(AtomicUsize::new(0));
        let rejected = guardian_lifetime_test_pane(
            724,
            rejected_identity,
            rejected_control.clone(),
            Arc::clone(&rejected_kills),
        );
        let rejected = crate::domain::UnpublishedPane::from_guardian_proxy(rejected).unwrap();
        let rejected_publication = rejected.guardian_publication_receipt().unwrap();
        assert!(
            rejected.publish(&mux).is_err(),
            "numeric pane collision must fail publication"
        );
        assert!(!rejected_publication.was_published());
        assert_eq!(
            rejected_control.retirement_attempts.load(Ordering::SeqCst),
            1
        );
        assert_eq!(
            rejected_control.retirement_effects.load(Ordering::SeqCst),
            1
        );
        assert_eq!(rejected_control.close_attempts.load(Ordering::SeqCst), 0);
        assert_eq!(rejected_kills.load(Ordering::SeqCst), 0);
        assert!(Arc::ptr_eq(&mux.get_pane(724).unwrap(), &published));
        assert_eq!(control.retirement_attempts.load(Ordering::SeqCst), 0);
        assert_eq!(control.close_attempts.load(Ordering::SeqCst), 0);
        assert_eq!(kills.load(Ordering::SeqCst), 0);
        drop(release);
        mux.remove_pane(724);
        assert!(mux.get_pane(724).is_none());
        drop(published);
        drop(mux);
        assert!(publication.was_published());
        assert!(publication_clone.was_published());
        assert!(!rejected_publication.was_published());
    }

    #[test]
    fn guardian_explicit_close_is_single_shot_and_suppresses_drop_retirement() {
        let identity = guardian_lifetime_test_identity(1);
        let control = Arc::new(FencedGuardianLeaseControl::new(identity));
        let kills = Arc::new(AtomicUsize::new(0));
        let pane = guardian_lifetime_test_pane(702, identity, control.clone(), Arc::clone(&kills));

        Pane::kill(&pane);
        Pane::kill(&pane);
        drop(pane);

        assert_eq!(control.close_attempts.load(Ordering::SeqCst), 1);
        assert_eq!(control.close_effects.load(Ordering::SeqCst), 1);
        assert_eq!(control.retirement_attempts.load(Ordering::SeqCst), 0);
        assert_eq!(kills.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn stale_guardian_generation_cannot_mutate_or_retire_same_id_successor() {
        let stale_identity = guardian_lifetime_test_identity(1);
        let successor_identity = guardian_lifetime_test_identity(2);
        assert_eq!(stale_identity.pane_id(), successor_identity.pane_id());
        let control = Arc::new(FencedGuardianLeaseControl::new(successor_identity));

        let stale_close_kills = Arc::new(AtomicUsize::new(0));
        let stale_close = guardian_lifetime_test_pane(
            703,
            stale_identity,
            control.clone(),
            Arc::clone(&stale_close_kills),
        );
        Pane::kill(&stale_close);
        drop(stale_close);
        assert_eq!(
            control.retirement_attempts.load(Ordering::SeqCst),
            0,
            "an indeterminate or rejected close must not be followed by takeover-enabling retirement",
        );

        let stale_retire_kills = Arc::new(AtomicUsize::new(0));
        drop(guardian_lifetime_test_pane(
            704,
            stale_identity,
            control.clone(),
            Arc::clone(&stale_retire_kills),
        ));

        assert_eq!(control.close_attempts.load(Ordering::SeqCst), 1);
        assert_eq!(control.close_effects.load(Ordering::SeqCst), 0);
        assert_eq!(control.retirement_attempts.load(Ordering::SeqCst), 1);
        assert_eq!(control.retirement_effects.load(Ordering::SeqCst), 0);
        assert_eq!(stale_close_kills.load(Ordering::SeqCst), 0);
        assert_eq!(stale_retire_kills.load(Ordering::SeqCst), 0);

        let successor_kills = Arc::new(AtomicUsize::new(0));
        let successor = guardian_lifetime_test_pane(
            705,
            successor_identity,
            control.clone(),
            Arc::clone(&successor_kills),
        );
        Pane::kill(&successor);
        drop(successor);

        assert_eq!(control.close_attempts.load(Ordering::SeqCst), 2);
        assert_eq!(control.close_effects.load(Ordering::SeqCst), 1);
        assert_eq!(control.retirement_attempts.load(Ordering::SeqCst), 1);
        assert_eq!(control.retirement_effects.load(Ordering::SeqCst), 0);
        assert_eq!(successor_kills.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn proc_list_warm_pending_guard_is_single_flight_and_releases_on_drop() {
        let pending = Arc::new(AtomicBool::new(false));
        let guard = ProcListWarmPendingGuard::try_acquire(&pending)
            .expect("idle warm flag should admit one worker");

        assert!(pending.load(Ordering::Acquire));
        assert!(
            ProcListWarmPendingGuard::try_acquire(&pending).is_none(),
            "a live guard must reject a second worker",
        );

        drop(guard);
        assert!(!pending.load(Ordering::Acquire));
        assert!(
            ProcListWarmPendingGuard::try_acquire(&pending).is_some(),
            "dropping the guard must make a later warm retryable",
        );
    }

    #[test]
    fn proc_list_warm_pending_guard_releases_during_unwind() {
        let pending = Arc::new(AtomicBool::new(false));
        let pending_for_unwind = Arc::clone(&pending);

        let result = std::panic::catch_unwind(move || {
            let _guard = ProcListWarmPendingGuard::try_acquire(&pending_for_unwind)
                .expect("idle warm flag should admit the panicking worker");
            panic!("intentional process-cache warm panic");
        });

        assert!(result.is_err());
        assert!(
            !pending.load(Ordering::Acquire),
            "unwinding a warm worker must release its single-flight admission",
        );
    }

    #[test]
    fn child_exit_prune_tracker_preserves_exit_before_registration() {
        let mut tracker = ChildExitPruneTracker::default();
        tracker.record_child_exit();

        assert!(tracker.has_pending_intent());
        assert!(
            tracker.record_registration_bound(),
            "binding after exit must request a prune"
        );
        let post_bind_intent = Arc::clone(tracker.current_intent.as_ref().unwrap());
        tracker.record_success(&post_bind_intent);
        assert!(
            !tracker.has_pending_intent(),
            "a prune for the post-bind intent must consume the earlier exit"
        );
    }

    #[test]
    fn child_exit_prune_tracker_ignores_registration_before_exit() {
        let mut tracker = ChildExitPruneTracker::default();

        assert!(
            !tracker.record_registration_bound(),
            "a live child needs no prune at publication"
        );
        assert!(!tracker.has_pending_intent());

        tracker.record_child_exit();
        assert!(
            tracker.has_pending_intent(),
            "a later child exit must create the prune intent"
        );
    }

    #[test]
    fn child_exit_prune_tracker_does_not_consume_concurrent_rebind() {
        let mut tracker = ChildExitPruneTracker::default();
        tracker.record_child_exit();
        let first_generation_intent = Arc::clone(tracker.current_intent.as_ref().unwrap());

        assert!(tracker.record_registration_bound());
        let replacement_generation_intent = Arc::clone(tracker.current_intent.as_ref().unwrap());
        assert!(!Arc::ptr_eq(
            &first_generation_intent,
            &replacement_generation_intent
        ));
        tracker.record_success(&first_generation_intent);

        assert!(
            tracker.has_pending_intent(),
            "completion for an old generation must preserve a newer bind intent"
        );
        tracker.record_success(&replacement_generation_intent);
        assert!(!tracker.has_pending_intent());
    }

    #[test]
    fn child_exit_prune_dispatch_drop_releases_schedule_and_preserves_intent() {
        let registration = Arc::new(PaneRegistrationSlot::default());
        let state = ChildExitPruneState::new(registration);
        {
            let mut tracker = state.tracker.lock();
            tracker.record_child_exit();
            tracker.scheduled = true;
        }

        drop(ChildExitPruneDispatch {
            state: Arc::clone(&state),
            registration: None,
            target_intent: Arc::new(()),
            finished: false,
        });

        let tracker = state.tracker.lock();
        assert!(
            !tracker.scheduled,
            "dropping a rejected runnable must release single-flight admission"
        );
        assert!(
            tracker.has_pending_intent(),
            "scheduler rejection must not consume the child-exit intent"
        );
    }

    #[derive(Default)]
    struct ResizeReplayHarness {
        queue: ResizeQueueState,
        in_flight: Option<PendingResize>,
        presented_seq: Option<u64>,
        presented_size: Option<TerminalSize>,
        completed: Vec<u64>,
        cancelled: Vec<u64>,
        rejected_frames: Vec<u64>,
        causality: Vec<String>,
    }

    impl ResizeReplayHarness {
        fn enqueue(&mut self, cols: usize, rows: usize) -> ResizeEnqueueOutcome {
            let size = term_size(cols, rows);
            let pty = pty_size(cols as u16, rows as u16);
            let outcome = self.queue.enqueue(size, pty, Instant::now());
            self.causality.push(format!(
                "intent seq={} target={}x{} replaced_seq={:?} spawn_worker={}",
                outcome.seq, cols, rows, outcome.replaced_seq, outcome.spawn_worker
            ));
            outcome
        }

        fn start_next(&mut self) -> Option<PendingResize> {
            if self.in_flight.is_some() {
                return None;
            }

            let pending = self.queue.dequeue_for_worker();
            if let Some(pending) = pending {
                self.causality.push(format!(
                    "start seq={} target={}x{}",
                    pending.seq, pending.size.cols, pending.size.rows
                ));
                self.in_flight = Some(pending);
            }
            pending
        }

        fn complete_current(&mut self) -> Option<PendingResize> {
            let completed = self.in_flight.take()?;
            self.causality.push(format!(
                "complete seq={} target={}x{}",
                completed.seq, completed.size.cols, completed.size.rows
            ));
            self.completed.push(completed.seq);
            Some(completed)
        }

        fn commit_current_with_present_barrier(&mut self) -> Option<bool> {
            let active = self.in_flight?;
            let token = ResizeCancellationToken::new(active.seq);

            if let Some(superseded_by_seq) = self.queue.superseded_by(token) {
                let rejected = self.in_flight.take().expect("in-flight resize must exist");
                self.cancelled.push(rejected.seq);
                self.rejected_frames.push(rejected.seq);
                self.causality.push(format!(
                    "reject_frame commit_id={} superseded_by={} swap_barrier_wait_us={}",
                    rejected.seq, superseded_by_seq, 0
                ));
                return Some(false);
            }

            let committed = self.complete_current()?;
            self.presented_seq = Some(committed.seq);
            self.presented_size = Some(committed.size);
            self.causality.push(format!(
                "commit_frame commit_id={} rejected_frame=false swap_barrier_wait_us={}",
                committed.seq, 0
            ));
            Some(true)
        }

        fn boundary_cancel_current_if_superseded(&mut self) -> bool {
            let active = match self.in_flight {
                Some(active) => active,
                None => return false,
            };

            let token = ResizeCancellationToken::new(active.seq);
            let Some(latest_seq) = self.queue.superseded_by(token) else {
                return false;
            };

            let cancelled = self.in_flight.take().expect("in-flight resize must exist");
            self.cancelled.push(cancelled.seq);
            self.causality.push(format!(
                "cancel seq={} superseded_by={latest_seq}",
                cancelled.seq
            ));
            true
        }

        fn causality_contains(&self, needle: &str) -> bool {
            self.causality.iter().any(|line| line.contains(needle))
        }
    }

    #[test]
    fn retry_with_backoff_succeeds_after_transient_failures() {
        let policy = ResizeRetryPolicy {
            max_attempts: 5,
            base_backoff: Duration::default(),
            max_backoff: Duration::default(),
        };
        let mut seen_attempts = Vec::new();

        let result = retry_with_backoff(policy, |attempt| {
            seen_attempts.push(attempt);
            if attempt < 3 {
                Err("transient")
            } else {
                Ok("ok")
            }
        });

        let (value, stats) = result.expect("retry should eventually succeed");
        assert_eq!(value, "ok");
        assert_eq!(stats.attempts, 3);
        assert_eq!(stats.backoff_elapsed, Duration::default());
        assert_eq!(seen_attempts, vec![1, 2, 3]);
    }

    #[test]
    fn retry_with_backoff_reports_terminal_failure_after_budget() {
        let policy = ResizeRetryPolicy {
            max_attempts: 3,
            base_backoff: Duration::default(),
            max_backoff: Duration::default(),
        };
        let mut seen_attempts = 0usize;

        let result: Result<(&'static str, ResizeRetryStats), (&'static str, ResizeRetryStats)> =
            retry_with_backoff(policy, |_| {
                seen_attempts += 1;
                Err("persistent")
            });

        let (err, stats) = result.expect_err("retry should fail after max attempts");
        assert_eq!(err, "persistent");
        assert_eq!(stats.attempts, 3);
        assert_eq!(stats.backoff_elapsed, Duration::default());
        assert_eq!(seen_attempts, 3);
    }

    #[test]
    fn controlled_retry_stops_without_sleeping_or_invoking_later_attempts() {
        let policy = ResizeRetryPolicy {
            max_attempts: 5,
            base_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(1),
        };
        let mut seen_attempts = Vec::new();

        let result: Result<(&'static str, ResizeRetryStats), (&'static str, ResizeRetryStats)> =
            retry_with_backoff_controlled(policy, |attempt| {
                seen_attempts.push(attempt);
                Err(RetryStepError::Stop("superseded"))
            });

        let (err, stats) = result.expect_err("stop directive must terminate retry immediately");
        assert_eq!(err, "superseded");
        assert_eq!(stats.attempts, 1);
        assert_eq!(stats.backoff_elapsed, Duration::default());
        assert_eq!(seen_attempts, vec![1]);
    }

    #[test]
    fn controlled_retry_does_not_apply_stale_resize_after_supersession() {
        let policy = ResizeRetryPolicy {
            max_attempts: 5,
            base_backoff: Duration::default(),
            max_backoff: Duration::default(),
        };
        let queue = Mutex::new(ResizeQueueState::default());
        let first = queue
            .lock()
            .enqueue(term_size(80, 24), pty_size(80, 24), Instant::now());
        queue
            .lock()
            .dequeue_for_worker()
            .expect("first intent must enter the worker");
        let token = ResizeCancellationToken::new(first.seq);
        let mut simulated_pty_calls = 0usize;

        let result: Result<((), ResizeRetryStats), (&'static str, ResizeRetryStats)> =
            retry_with_backoff_controlled(policy, |attempt| {
                if queue.lock().superseded_by(token).is_some() {
                    return Err(RetryStepError::Stop("superseded"));
                }
                simulated_pty_calls += 1;
                if attempt == 1 {
                    queue
                        .lock()
                        .enqueue(term_size(120, 40), pty_size(120, 40), Instant::now());
                    return Err(RetryStepError::Retry("transient apply failure"));
                }
                Ok(())
            });

        let (err, stats) = result.expect_err("newer intent must stop the stale retry loop");
        assert_eq!(err, "superseded");
        assert_eq!(stats.attempts, 2);
        assert_eq!(simulated_pty_calls, 1);
        assert_eq!(
            queue.lock().pending.as_ref().map(|intent| intent.seq),
            Some(2)
        );
    }

    #[test]
    fn retry_with_backoff_treats_zero_attempt_budget_as_one_attempt() {
        let policy = ResizeRetryPolicy {
            max_attempts: 0,
            base_backoff: Duration::default(),
            max_backoff: Duration::default(),
        };
        let mut seen_attempts = 0usize;

        let result: Result<(&'static str, ResizeRetryStats), (&'static str, ResizeRetryStats)> =
            retry_with_backoff(policy, |_| {
                seen_attempts += 1;
                Err("persistent")
            });

        let (err, stats) = result.expect_err("zero-attempt budget should still try once");
        assert_eq!(err, "persistent");
        assert_eq!(stats.attempts, 1);
        assert_eq!(stats.backoff_elapsed, Duration::default());
        assert_eq!(seen_attempts, 1);
    }

    #[test]
    fn retry_backoff_is_monotonic_and_capped() {
        let policy = ResizeRetryPolicy {
            max_attempts: 6,
            base_backoff: Duration::from_millis(2),
            max_backoff: Duration::from_millis(5),
        };

        let d1 = retry_backoff_for_attempt(policy, 1);
        let d2 = retry_backoff_for_attempt(policy, 2);
        let d3 = retry_backoff_for_attempt(policy, 3);
        let d4 = retry_backoff_for_attempt(policy, 4);

        assert!(d1 <= d2);
        assert!(d2 <= d3);
        assert!(d3 <= d4);
        assert_eq!(d1, Duration::from_millis(2));
        assert_eq!(d2, Duration::from_millis(4));
        assert_eq!(d3, Duration::from_millis(5));
        assert_eq!(d4, Duration::from_millis(5));
    }

    #[test]
    fn retry_backoff_accounting_saturates_duration_overflow() {
        // Verify the backoff accounting saturates at `Duration::MAX` rather than
        // overflow-panicking. This drives `retry_backoff_for_attempt` and the
        // `saturating_add` accumulation directly (the exact ops `retry_with_backoff`
        // performs at lib.rs ~324-325) instead of running the real retry loop:
        // with `base_backoff = Duration::MAX` that loop would `thread::sleep`
        // `Duration::MAX` between attempts and hang the test forever.
        let policy = ResizeRetryPolicy {
            max_attempts: 3,
            base_backoff: Duration::MAX,
            max_backoff: Duration::MAX,
        };

        // Per-attempt backoff must saturate at MAX (no overflow in saturating_mul).
        assert_eq!(retry_backoff_for_attempt(policy, 1), Duration::MAX);
        assert_eq!(retry_backoff_for_attempt(policy, 2), Duration::MAX);

        // The retry loop sleeps (and accounts) the backoff for every attempt except
        // the last, so accumulate attempts 1..max_attempts and confirm the running
        // total saturates at MAX instead of panicking.
        let mut backoff_elapsed = Duration::default();
        let mut attempts = 0;
        for attempt in 1..policy.max_attempts {
            attempts = attempt;
            backoff_elapsed =
                backoff_elapsed.saturating_add(retry_backoff_for_attempt(policy, attempt));
        }
        // attempts loops over 1,2 (the sleeping attempts); the 3rd is the terminal
        // failure that `retry_with_backoff` reports without sleeping.
        assert_eq!(attempts, policy.max_attempts - 1);
        assert_eq!(backoff_elapsed, Duration::MAX);
    }

    #[test]
    fn search_end_grapheme_index_saturates() {
        assert_eq!(next_search_grapheme_idx(0), 1);
        assert_eq!(next_search_grapheme_idx(usize::MAX), usize::MAX);
    }

    #[test]
    fn next_resize_retry_attempt_saturates() {
        assert_eq!(next_resize_retry_attempt(1), 2);
        assert_eq!(next_resize_retry_attempt(usize::MAX), usize::MAX);
    }

    #[test]
    fn resize_queue_coalesces_latest_pending_when_worker_is_running() {
        let mut queue = ResizeQueueState::default();
        let now = Instant::now();

        let first = queue.enqueue(term_size(80, 24), pty_size(80, 24), now);
        assert_eq!(first.seq, 1);
        assert!(first.spawn_worker);
        assert_eq!(first.replaced_seq, None);
        assert_eq!(first.queue_depth_hint, 1);

        let in_flight = queue
            .dequeue_for_worker()
            .expect("first request must be available for worker");
        assert_eq!(in_flight.seq, 1);

        let second = queue
            .try_enqueue(term_size(100, 30), pty_size(100, 30), now, true, None)
            .unwrap();
        assert!(queue.reconcile_tab_on_completion);
        assert_eq!(second.seq, 2);
        assert!(!second.spawn_worker);
        assert_eq!(second.replaced_seq, None);
        assert_eq!(second.queue_depth_hint, 2);

        let third = queue.enqueue(term_size(120, 40), pty_size(120, 40), now);
        assert!(
            !queue.reconcile_tab_on_completion,
            "a newer GUI-owned layout must revoke remote tab inference",
        );
        assert_eq!(third.seq, 3);
        assert!(!third.spawn_worker);
        assert_eq!(third.replaced_seq, Some(2));
        assert_eq!(third.queue_depth_hint, 2);

        let next = queue
            .dequeue_for_worker()
            .expect("coalesced request must be available");
        assert_eq!(next.seq, 3);
        assert_eq!(next.size, term_size(120, 40));
        assert_eq!(next.pty_size, pty_size(120, 40));

        assert!(queue.dequeue_for_worker().is_none());
        assert!(!queue.worker_running);
    }

    #[test]
    fn resize_queue_marks_worker_idle_when_empty() {
        let mut queue = ResizeQueueState::default();
        let now = Instant::now();

        let first = queue.enqueue(term_size(90, 25), pty_size(90, 25), now);
        assert!(first.spawn_worker);
        assert!(queue.dequeue_for_worker().is_some());
        assert!(queue.worker_running);

        assert!(queue.dequeue_for_worker().is_none());
        assert!(!queue.worker_running);

        let second = queue.enqueue(term_size(91, 25), pty_size(91, 25), now);
        assert!(second.spawn_worker);
        assert_eq!(second.queue_depth_hint, 1);
    }

    #[test]
    fn resize_queue_stress_preserves_latest_intent_only() {
        let mut queue = ResizeQueueState::default();
        let now = Instant::now();

        let first = queue.enqueue(term_size(80, 24), pty_size(80, 24), now);
        assert!(first.spawn_worker);
        let _ = queue.dequeue_for_worker();

        for n in 0..1000u16 {
            let cols = 100 + n;
            let rows = 40 + (n % 10);
            let _ = queue.enqueue(
                term_size(cols as usize, rows as usize),
                pty_size(cols, rows),
                now,
            );
        }

        let pending = queue
            .dequeue_for_worker()
            .expect("latest coalesced request should remain");
        assert_eq!(pending.size.cols, 1099);
        assert_eq!(pending.size.rows, 49);
        assert_eq!(pending.pty_size.cols, 1099);
        assert_eq!(pending.pty_size.rows, 49);
    }

    #[test]
    fn resize_queue_cancellation_token_reports_when_intent_is_superseded() {
        let mut queue = ResizeQueueState::default();
        let now = Instant::now();

        let first = queue.enqueue(term_size(80, 24), pty_size(80, 24), now);
        let token = ResizeCancellationToken::new(first.seq);
        assert_eq!(queue.superseded_by(token), None);

        let second = queue.enqueue(term_size(100, 30), pty_size(100, 30), now);
        assert_eq!(queue.superseded_by(token), Some(second.seq));
        assert_eq!(
            queue.superseded_by(ResizeCancellationToken::new(second.seq)),
            None
        );
    }

    #[test]
    fn resize_queue_rejects_sequence_exhaustion_without_mutating_authority() {
        let mut queue = ResizeQueueState {
            next_seq: u64::MAX - 1,
            ..ResizeQueueState::default()
        };
        let now = Instant::now();

        let max = queue.enqueue(term_size(80, 24), pty_size(80, 24), now);
        assert_eq!(max.seq, u64::MAX);
        let max_token = ResizeCancellationToken::new(max.seq);
        assert_eq!(queue.superseded_by(max_token), None);
        queue
            .dequeue_for_worker()
            .expect("max-generation intent must enter the worker");

        assert_eq!(
            queue.try_enqueue(term_size(120, 40), pty_size(120, 40), now, false, None),
            Err(ResizeEnqueueError::SequenceExhausted),
            "generation exhaustion must fail closed rather than alias zero",
        );
        assert_eq!(queue.next_seq, u64::MAX);
        assert!(queue.pending.is_none());
        assert!(queue.worker_running);
        assert_eq!(
            queue.superseded_by(max_token),
            None,
            "a rejected resize must not supersede the admitted max generation",
        );
    }

    #[test]
    fn supersession_back_to_terminal_size_still_requires_pty_reconciliation() {
        let terminal_a = term_size(80, 24);
        let pty_a = pty_size(80, 24);
        let terminal_b = term_size(120, 40);
        let pty_b = pty_size(120, 40);
        let mut queue = ResizeQueueState::default();

        let first = queue.enqueue(terminal_b, pty_b, Instant::now());
        queue
            .dequeue_for_worker()
            .expect("first intent must enter the worker");
        // Model the first intent completing its PTY resize before it loses the
        // terminal-present race to a newer request that returns to size A.
        queue.last_proven_pty_size = Some(pty_b);
        let winner = queue.enqueue(terminal_a, pty_a, Instant::now());
        assert_eq!(
            queue.superseded_by(ResizeCancellationToken::new(first.seq)),
            Some(winner.seq),
        );

        let winning_intent = queue
            .dequeue_for_worker()
            .expect("winning return-to-A intent must remain queued");
        assert_eq!(winning_intent.seq, winner.seq);
        assert!(
            !resize_is_proven_noop(
                terminal_a,
                winning_intent.size,
                queue.last_proven_pty_size,
                winning_intent.pty_size,
            ),
            "terminal equality must not hide the PTY left at superseded size B",
        );

        queue.last_proven_pty_size = Some(pty_a);
        assert!(resize_is_proven_noop(
            terminal_a,
            winning_intent.size,
            queue.last_proven_pty_size,
            winning_intent.pty_size,
        ));
    }

    #[test]
    fn replay_cancellation_race_coalesces_to_latest_intent() {
        let mut replay = ResizeReplayHarness::default();

        let first = replay.enqueue(80, 24);
        assert!(first.spawn_worker);
        let in_flight = replay.start_next().expect("first intent should start");
        assert_eq!(in_flight.seq, 1);

        let second = replay.enqueue(120, 30);
        assert_eq!(second.replaced_seq, None);
        let third = replay.enqueue(140, 40);
        assert_eq!(third.replaced_seq, Some(2));

        assert!(replay.boundary_cancel_current_if_superseded());
        let coalesced = replay
            .start_next()
            .expect("latest coalesced intent should start");
        assert_eq!(coalesced.seq, 3);
        replay
            .complete_current()
            .expect("coalesced intent should complete");

        assert_eq!(replay.cancelled, vec![1]);
        assert_eq!(replay.completed, vec![3]);
        assert!(replay.causality_contains("intent seq=3"));
        assert!(replay.causality_contains("replaced_seq=Some(2)"));
        assert!(replay.causality_contains("cancel seq=1 superseded_by=3"));
        assert!(replay.causality_contains("complete seq=3"));
    }

    #[test]
    fn replay_prevents_out_of_order_completion() {
        let mut replay = ResizeReplayHarness::default();

        replay.enqueue(90, 30);
        replay.start_next().expect("first intent should start");

        replay.enqueue(100, 30);
        replay.enqueue(110, 30);

        // Worker has one in-flight request; next start attempt must be deferred.
        assert!(replay.start_next().is_none());

        let first_complete = replay.complete_current().expect("first should complete");
        assert_eq!(first_complete.seq, 1);

        let second_start = replay
            .start_next()
            .expect("latest pending should now start");
        assert_eq!(second_start.seq, 3);
        replay
            .complete_current()
            .expect("second in-flight should complete");

        assert_eq!(replay.completed, vec![1, 3]);
    }

    #[test]
    fn replay_rapid_resizes_emit_intent_to_completion_causality_chain() {
        let mut replay = ResizeReplayHarness::default();

        replay.enqueue(80, 24);
        replay.start_next().expect("first intent should start");

        for i in 0..200usize {
            let _ = replay.enqueue(100 + i, 30 + (i % 5));
        }

        replay.complete_current().expect("first should complete");
        let latest = replay.start_next().expect("latest pending should start");
        replay.complete_current().expect("latest should complete");

        assert!(latest.seq > 1);
        assert!(replay.causality_contains("intent seq=1"));
        assert!(replay.causality_contains("start seq=1"));
        assert!(replay.causality_contains("complete seq=1"));
        assert!(
            replay
                .causality
                .iter()
                .any(|line| line.contains("replaced_seq=Some(")),
            "expected at least one coalescing replacement entry"
        );
        assert!(replay.causality_contains(&format!("complete seq={}", latest.seq)));
    }

    #[test]
    fn replay_present_commit_barrier_rejects_superseded_commit() {
        let mut replay = ResizeReplayHarness::default();

        replay.enqueue(80, 24);
        let started = replay.start_next().expect("first intent should start");
        assert_eq!(started.seq, 1);

        replay.enqueue(120, 40);
        assert_eq!(
            replay.commit_current_with_present_barrier(),
            Some(false),
            "superseded frame should be rejected at present-commit barrier"
        );
        assert_eq!(replay.presented_seq, None);
        assert_eq!(replay.rejected_frames, vec![1]);
        assert!(replay.causality_contains("reject_frame commit_id=1 superseded_by=2"));

        let coalesced = replay.start_next().expect("latest intent should run");
        assert_eq!(coalesced.seq, 2);
        assert_eq!(replay.commit_current_with_present_barrier(), Some(true));
        assert_eq!(replay.presented_seq, Some(2));
        assert_eq!(
            replay.presented_size.map(|size| (size.cols, size.rows)),
            Some((120, 40))
        );
        assert!(replay.causality_contains("commit_frame commit_id=2 rejected_frame=false"));
    }

    #[test]
    fn replay_presented_frame_updates_only_on_commit() {
        let mut replay = ResizeReplayHarness::default();

        replay.enqueue(90, 30);
        replay.start_next().expect("first intent should start");
        assert_eq!(replay.presented_seq, None);
        assert_eq!(replay.presented_size, None);

        replay.enqueue(100, 35);
        assert_eq!(replay.commit_current_with_present_barrier(), Some(false));
        assert_eq!(
            replay.presented_seq, None,
            "rejected frame must not become visible"
        );
        assert_eq!(replay.presented_size, None);

        replay.start_next().expect("coalesced intent should start");
        assert_eq!(replay.presented_seq, None);
        assert_eq!(replay.commit_current_with_present_barrier(), Some(true));
        assert_eq!(replay.presented_seq, Some(2));
        assert_eq!(
            replay.presented_size.map(|size| (size.cols, size.rows)),
            Some((100, 35))
        );
    }

    #[test]
    fn replay_fallback_paths_preserve_identical_presented_outcome() {
        let mut boundary_cancel = ResizeReplayHarness::default();
        boundary_cancel.enqueue(80, 24);
        boundary_cancel
            .start_next()
            .expect("first intent should start");
        boundary_cancel.enqueue(120, 40);
        assert!(boundary_cancel.boundary_cancel_current_if_superseded());
        boundary_cancel
            .start_next()
            .expect("latest intent should start after boundary cancellation");
        assert_eq!(
            boundary_cancel.commit_current_with_present_barrier(),
            Some(true)
        );

        let mut present_reject = ResizeReplayHarness::default();
        present_reject.enqueue(80, 24);
        present_reject
            .start_next()
            .expect("first intent should start");
        present_reject.enqueue(120, 40);
        assert_eq!(
            present_reject.commit_current_with_present_barrier(),
            Some(false),
            "superseded in-flight should reject at present barrier"
        );
        present_reject
            .start_next()
            .expect("latest intent should start after reject");
        assert_eq!(
            present_reject.commit_current_with_present_barrier(),
            Some(true)
        );

        assert_eq!(
            boundary_cancel.presented_seq, present_reject.presented_seq,
            "presented sequence should be deterministic across fallback paths"
        );
        assert_eq!(
            boundary_cancel.presented_size.map(|s| (s.cols, s.rows)),
            present_reject.presented_size.map(|s| (s.cols, s.rows)),
            "presented geometry should be deterministic across fallback paths"
        );
        assert_eq!(
            boundary_cancel.completed, present_reject.completed,
            "completed commit ids should match across fallback paths"
        );
        assert_eq!(boundary_cancel.cancelled.len(), 1);
        assert_eq!(present_reject.cancelled.len(), 1);
        assert!(boundary_cancel.rejected_frames.is_empty());
        assert_eq!(present_reject.rejected_frames, vec![1]);
    }

    // =========================================================================
    // Additional resize queue and replay edge cases
    // =========================================================================

    #[test]
    fn queue_empty_dequeue_returns_none_and_marks_idle() {
        let mut queue = ResizeQueueState::default();
        // Never enqueued — dequeue should return None
        assert!(queue.dequeue_for_worker().is_none());
        assert!(!queue.worker_running);
    }

    #[test]
    fn queue_seq_monotonically_increases() {
        let mut queue = ResizeQueueState::default();
        let now = Instant::now();
        let mut prev_seq = 0u64;
        for i in 1..=50 {
            let outcome = queue.enqueue(
                term_size(80 + i, 24 + (i % 5)),
                pty_size((80 + i) as u16, (24 + (i % 5)) as u16),
                now,
            );
            assert!(
                outcome.seq > prev_seq,
                "seq should increase: {} > {}",
                outcome.seq,
                prev_seq
            );
            prev_seq = outcome.seq;
        }
    }

    #[test]
    fn queue_replaced_seq_chains_correctly() {
        let mut queue = ResizeQueueState::default();
        let now = Instant::now();

        // First enqueue — no replacement
        let o1 = queue.enqueue(term_size(80, 24), pty_size(80, 24), now);
        assert_eq!(o1.replaced_seq, None);

        // Take first as in-flight
        queue.dequeue_for_worker();

        // Second enqueue while first is running — no replacement (nothing pending)
        let o2 = queue.enqueue(term_size(90, 24), pty_size(90, 24), now);
        assert_eq!(o2.replaced_seq, None);

        // Third replaces second
        let o3 = queue.enqueue(term_size(100, 24), pty_size(100, 24), now);
        assert_eq!(o3.replaced_seq, Some(o2.seq));

        // Fourth replaces third
        let o4 = queue.enqueue(term_size(110, 24), pty_size(110, 24), now);
        assert_eq!(o4.replaced_seq, Some(o3.seq));
    }

    #[test]
    fn queue_worker_restart_after_idle() {
        let mut queue = ResizeQueueState::default();
        let now = Instant::now();

        // First cycle
        let o1 = queue.enqueue(term_size(80, 24), pty_size(80, 24), now);
        assert!(o1.spawn_worker);
        queue.dequeue_for_worker(); // process first
        queue.dequeue_for_worker(); // goes idle
        assert!(!queue.worker_running);

        // Second cycle — worker should spawn again
        let o2 = queue.enqueue(term_size(100, 30), pty_size(100, 30), now);
        assert!(o2.spawn_worker, "worker should respawn after going idle");
        assert_eq!(o2.queue_depth_hint, 1);
    }

    #[test]
    fn cancellation_token_for_latest_seq_is_not_superseded() {
        let mut queue = ResizeQueueState::default();
        let now = Instant::now();

        queue.enqueue(term_size(80, 24), pty_size(80, 24), now);
        queue.enqueue(term_size(90, 30), pty_size(90, 30), now);
        queue.enqueue(term_size(100, 40), pty_size(100, 40), now);

        // Token for the latest seq should NOT be superseded
        let latest_token = ResizeCancellationToken::new(3);
        assert_eq!(queue.superseded_by(latest_token), None);

        // Token for older seq should be superseded
        let old_token = ResizeCancellationToken::new(1);
        assert_eq!(queue.superseded_by(old_token), Some(3));
    }

    #[test]
    fn replay_single_intent_completes_cleanly() {
        let mut replay = ResizeReplayHarness::default();

        let o = replay.enqueue(80, 24);
        assert!(o.spawn_worker);

        replay.start_next().expect("intent should start");
        assert_eq!(
            replay.commit_current_with_present_barrier(),
            Some(true),
            "single intent commits successfully"
        );
        assert_eq!(replay.presented_seq, Some(1));
        assert_eq!(
            replay.presented_size.map(|s| (s.cols, s.rows)),
            Some((80, 24))
        );
        assert!(replay.cancelled.is_empty());
        assert!(replay.rejected_frames.is_empty());
    }

    #[test]
    fn replay_sequential_intents_without_overlap() {
        let mut replay = ResizeReplayHarness::default();

        // First intent — enqueue, start, commit
        replay.enqueue(80, 24);
        replay.start_next().unwrap();
        replay.commit_current_with_present_barrier();
        // Worker tries to dequeue again — nothing pending → goes idle
        assert!(
            replay.start_next().is_none(),
            "no more work after first commit"
        );

        // Second intent — worker went idle, new intent respawns
        let o2 = replay.enqueue(100, 30);
        assert!(
            o2.spawn_worker,
            "worker should respawn for second intent after idle"
        );
        replay.start_next().unwrap();
        assert_eq!(
            replay.commit_current_with_present_barrier(),
            Some(true),
            "second intent commits cleanly"
        );
        assert_eq!(replay.presented_seq, Some(2));
        assert!(replay.cancelled.is_empty());
    }

    #[test]
    fn replay_start_next_when_nothing_queued() {
        let mut replay = ResizeReplayHarness::default();
        assert!(
            replay.start_next().is_none(),
            "start_next on empty queue returns None"
        );
    }

    #[test]
    fn replay_commit_with_no_in_flight_returns_none() {
        let mut replay = ResizeReplayHarness::default();
        assert_eq!(
            replay.commit_current_with_present_barrier(),
            None,
            "commit with no in-flight should return None"
        );
    }

    #[test]
    fn replay_cancel_with_no_in_flight_returns_false() {
        let mut replay = ResizeReplayHarness::default();
        assert!(
            !replay.boundary_cancel_current_if_superseded(),
            "cancel with no in-flight should return false"
        );
    }

    #[test]
    fn replay_cancel_not_superseded_returns_false() {
        let mut replay = ResizeReplayHarness::default();
        replay.enqueue(80, 24);
        replay.start_next().unwrap();
        // No newer intent — cancel should not trigger
        assert!(
            !replay.boundary_cancel_current_if_superseded(),
            "cancel when not superseded should return false"
        );
    }

    #[test]
    fn replay_multi_cancel_cascade() {
        let mut replay = ResizeReplayHarness::default();

        // First intent starts
        replay.enqueue(80, 24);
        replay.start_next().unwrap();

        // Multiple rapid intents supersede it
        replay.enqueue(90, 25);
        replay.enqueue(100, 30);
        replay.enqueue(110, 35);

        // Cancel first — superseded by seq 4
        assert!(replay.boundary_cancel_current_if_superseded());
        assert_eq!(replay.cancelled, vec![1]);

        // Start and commit the latest coalesced
        let latest = replay.start_next().unwrap();
        assert_eq!(latest.seq, 4);
        assert_eq!(replay.commit_current_with_present_barrier(), Some(true));
        assert_eq!(replay.presented_seq, Some(4));
        assert_eq!(
            replay.presented_size.map(|s| (s.cols, s.rows)),
            Some((110, 35))
        );
    }

    #[test]
    fn replay_causality_log_covers_full_lifecycle() {
        let mut replay = ResizeReplayHarness::default();

        replay.enqueue(80, 24);
        replay.start_next().unwrap();
        replay.enqueue(120, 40);
        replay.commit_current_with_present_barrier(); // rejected
        replay.start_next().unwrap();
        replay.commit_current_with_present_barrier(); // committed

        // Verify causality log has all phases
        assert!(replay.causality_contains("intent seq=1"));
        assert!(replay.causality_contains("start seq=1"));
        assert!(replay.causality_contains("reject_frame commit_id=1"));
        assert!(replay.causality_contains("intent seq=2"));
        assert!(replay.causality_contains("start seq=2"));
        assert!(replay.causality_contains("commit_frame commit_id=2"));
    }

    #[test]
    fn queue_depth_hint_reflects_worker_state() {
        let mut queue = ResizeQueueState::default();
        let now = Instant::now();

        // Idle worker — depth is 1
        let o1 = queue.enqueue(term_size(80, 24), pty_size(80, 24), now);
        assert_eq!(o1.queue_depth_hint, 1);

        // Take in-flight, now worker is running
        queue.dequeue_for_worker();

        // With worker running — depth is 2 (1 in-flight + 1 pending)
        let o2 = queue.enqueue(term_size(90, 24), pty_size(90, 24), now);
        assert_eq!(o2.queue_depth_hint, 2);

        // Coalescing doesn't change depth hint
        let o3 = queue.enqueue(term_size(100, 24), pty_size(100, 24), now);
        assert_eq!(o3.queue_depth_hint, 2);
    }

    #[test]
    fn resize_worker_spawn_failure_settles_the_latest_retained_intent_inline() {
        let queue = Arc::new(Mutex::new(ResizeQueueState::default()));
        {
            let mut queue = queue.lock();
            let first = queue.enqueue(term_size(80, 24), pty_size(80, 24), Instant::now());
            assert!(first.spawn_worker);
            let latest = queue.enqueue(term_size(120, 40), pty_size(120, 40), Instant::now());
            assert!(!latest.spawn_worker);
            assert_eq!(latest.replaced_seq, Some(first.seq));
        }

        let settled = Arc::new(Mutex::new(Vec::new()));
        let queue_for_fallback = Arc::clone(&queue);
        let settled_for_fallback = Arc::clone(&settled);
        settle_resize_worker_spawn(Err::<(), _>("injected spawn failure"), move || {
            while let Some(pending) = queue_for_fallback.lock().dequeue_for_worker() {
                settled_for_fallback
                    .lock()
                    .push((pending.seq, pending.size));
            }
        });

        let settled = settled.lock();
        assert_eq!(settled.len(), 1);
        assert_eq!(settled[0].0, 2);
        assert_eq!(settled[0].1, term_size(120, 40));
        let queue = queue.lock();
        assert!(queue.pending.is_none());
        assert!(
            !queue.worker_running,
            "inline settlement must release worker admission after draining",
        );
    }

    #[test]
    fn resize_worker_spawn_success_does_not_run_inline_fallback() {
        let ran_inline = Arc::new(AtomicBool::new(false));
        let ran_inline_for_fallback = Arc::clone(&ran_inline);

        settle_resize_worker_spawn(Ok::<(), &str>(()), move || {
            ran_inline_for_fallback.store(true, Ordering::Release);
        });

        assert!(!ran_inline.load(Ordering::Acquire));
    }

    #[test]
    fn resize_intent_catch_requeues_dequeued_latest_target_after_panic() {
        let queue = Mutex::new(ResizeQueueState::default());
        let initial = queue
            .lock()
            .enqueue(term_size(80, 24), pty_size(80, 24), Instant::now());
        let pending = queue
            .lock()
            .dequeue_for_worker()
            .expect("initial intent must enter the worker");

        let outcome: Result<(), ResizeFailureRecovery> =
            catch_resize_intent(&queue, pending, || panic!("injected resize callback panic"));

        assert_eq!(outcome, Err(ResizeFailureRecovery::Requeued { retry: 1 }));
        let queue = queue.lock();
        let retained = queue
            .pending
            .expect("caught panic must preserve the dequeued latest target");
        assert_eq!(retained.seq, initial.seq);
        assert_eq!(retained.size, term_size(80, 24));
        assert_eq!(retained.recoverable_panic_retries, 1);
        assert_eq!(retained.apply_error_retries, 0);
        assert!(queue.worker_running);
    }

    #[test]
    fn resize_worker_panic_recovery_retains_latest_intent_without_replacement() {
        let mut queue = ResizeQueueState::default();
        let initial = queue.enqueue(term_size(80, 24), pty_size(80, 24), Instant::now());
        assert!(initial.spawn_worker);
        let mut intent = queue
            .dequeue_for_worker()
            .expect("initial intent must enter the worker");

        for retry in 1..=MAX_RESIZE_RECOVERABLE_PANIC_RETRIES {
            assert_eq!(
                queue.recover_failed_intent(intent, ResizeFailureKind::RecoverablePanic),
                ResizeFailureRecovery::Requeued { retry },
            );
            assert!(queue.worker_running);
            intent = queue
                .dequeue_for_worker()
                .expect("recoverable panic must requeue the exact latest intent");
            assert_eq!(intent.seq, initial.seq);
            assert_eq!(intent.size, term_size(80, 24));
            assert_eq!(intent.recoverable_panic_retries, retry);
        }

        assert_eq!(
            queue.recover_failed_intent(intent, ResizeFailureKind::RecoverablePanic),
            ResizeFailureRecovery::ExhaustedRetained {
                retries: MAX_RESIZE_RECOVERABLE_PANIC_RETRIES,
            },
        );
        let retained = queue
            .pending
            .expect("exhaustion must retain rather than forget the last requested target");
        assert_eq!(retained.seq, initial.seq);
        assert_eq!(retained.size, term_size(80, 24));
        assert!(!queue.worker_running);
    }

    #[test]
    fn resize_worker_panic_recovery_prefers_newer_pending_intent() {
        let mut queue = ResizeQueueState::default();
        let initial = queue.enqueue(term_size(80, 24), pty_size(80, 24), Instant::now());
        let panicked = queue
            .dequeue_for_worker()
            .expect("initial intent must enter the worker");
        let newer = queue.enqueue(term_size(120, 40), pty_size(120, 40), Instant::now());
        assert!(!newer.spawn_worker);

        assert_eq!(
            queue.recover_failed_intent(panicked, ResizeFailureKind::RecoverablePanic),
            ResizeFailureRecovery::Superseded { by_seq: newer.seq },
        );
        let retained = queue
            .pending
            .expect("newer target must remain admitted after older callback panic");
        assert_eq!(retained.seq, newer.seq);
        assert_eq!(retained.size, term_size(120, 40));
        assert_eq!(retained.recoverable_panic_retries, 0);
        assert_eq!(retained.apply_error_retries, 0);
        assert!(queue.worker_running);
        assert_ne!(initial.seq, newer.seq);
    }

    #[test]
    fn resize_worker_apply_error_retries_then_retains_exact_target() {
        let queue = Mutex::new(ResizeQueueState::default());
        let initial = queue
            .lock()
            .enqueue(term_size(80, 24), pty_size(80, 24), Instant::now());
        let first_attempt = queue
            .lock()
            .dequeue_for_worker()
            .expect("initial intent must enter the worker");

        let first_result: Result<(), (&str, ResizeFailureRecovery)> =
            recover_resize_apply_error(&queue, first_attempt, Err("injected apply error"));
        assert_eq!(
            first_result,
            Err((
                "injected apply error",
                ResizeFailureRecovery::Requeued { retry: 1 },
            )),
        );

        let second_attempt = queue
            .lock()
            .dequeue_for_worker()
            .expect("ordinary apply error must requeue the exact latest intent");
        assert_eq!(second_attempt.seq, initial.seq);
        assert_eq!(second_attempt.size, term_size(80, 24));
        assert_eq!(second_attempt.recoverable_panic_retries, 0);
        assert_eq!(second_attempt.apply_error_retries, 1);

        let second_result: Result<(), (&str, ResizeFailureRecovery)> =
            recover_resize_apply_error(&queue, second_attempt, Err("persistent apply error"));
        assert_eq!(
            second_result,
            Err((
                "persistent apply error",
                ResizeFailureRecovery::ExhaustedRetained {
                    retries: MAX_RESIZE_APPLY_ERROR_RETRIES,
                },
            )),
        );

        let queue = queue.lock();
        let retained = queue
            .pending
            .expect("retry exhaustion must retain the last requested geometry");
        assert_eq!(retained.seq, initial.seq);
        assert_eq!(retained.size, term_size(80, 24));
        assert_eq!(retained.apply_error_retries, MAX_RESIZE_APPLY_ERROR_RETRIES);
        assert!(!queue.worker_running);
    }

    #[test]
    fn resize_worker_apply_error_prefers_newer_pending_intent() {
        let queue = Mutex::new(ResizeQueueState::default());
        let initial = queue
            .lock()
            .enqueue(term_size(80, 24), pty_size(80, 24), Instant::now());
        let failed = queue
            .lock()
            .dequeue_for_worker()
            .expect("initial intent must enter the worker");
        let newer = queue
            .lock()
            .enqueue(term_size(120, 40), pty_size(120, 40), Instant::now());

        let result: Result<(), (&str, ResizeFailureRecovery)> =
            recover_resize_apply_error(&queue, failed, Err("injected apply error"));
        assert_eq!(
            result,
            Err((
                "injected apply error",
                ResizeFailureRecovery::Superseded { by_seq: newer.seq },
            )),
        );
        let queue = queue.lock();
        let retained = queue
            .pending
            .expect("newer target must survive an older intent's apply error");
        assert_eq!(retained.seq, newer.seq);
        assert_eq!(retained.size, term_size(120, 40));
        assert_eq!(retained.apply_error_retries, 0);
        assert!(queue.worker_running);
        assert_ne!(initial.seq, newer.seq);
    }

    #[test]
    fn tab_resize_admission_does_not_wait_for_ordinary_pane_terminal() {
        let pane = Arc::new(LocalPane::new(
            703,
            guardian_lifetime_test_terminal(),
            Box::new(KillCountingChild {
                kills: Arc::new(AtomicUsize::new(0)),
            }),
            Box::new(GuardianLifetimeTestMasterPty),
            Box::new(Vec::<u8>::new()),
            1,
            [0x73; 16],
            "resize-admission-test".to_string(),
        ));
        let tab = Arc::new(crate::tab::Tab::new(&term_size(80, 24)));
        let dynamic_pane: Arc<dyn Pane> = pane.clone();
        tab.assign_pane(&dynamic_pane);

        // Reproduce a parser holding the real terminal mutex during a spill.
        // Planning and queue admission must return before that lock is freed;
        // the existing resize worker is allowed to wait for model ownership.
        let terminal = pane.terminal.lock();
        let target = term_size(120, 30);
        let (started_tx, started_rx) = sync_channel(1);
        let (done_tx, done_rx) = sync_channel(1);
        let resizing_tab = Arc::clone(&tab);
        let resize = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            resizing_tab.resize(target);
            done_tx.send(resizing_tab.get_size()).unwrap();
        });
        started_rx.recv().unwrap();
        let admitted = done_rx.recv_timeout(Duration::from_secs(2));
        // Always release contention and join, including the regressed path.
        drop(terminal);
        resize.join().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while pane.resize_queue.lock().worker_running {
            assert!(Instant::now() < deadline, "resize worker did not settle");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            admitted.expect("ordinary pane layout waited for the terminal mutex"),
            target,
        );
        assert_eq!(pane.terminal.lock().get_size(), target);
    }

    #[test]
    fn resize_preparation_releases_locks_and_observes_supersession() {
        let terminal = Mutex::new(Terminal::new(
            term_size(80, 3),
            Arc::new(GuardianLifetimeTestTermConfig),
            "FrankenTerm",
            "resize-preparation-test",
            Box::new(std::io::sink()),
        ));
        terminal
            .lock()
            .advance_bytes(b"a long logical line that needs wrapping\r\nsecond line");
        let queue = Mutex::new(ResizeQueueState::default());
        let target = term_size(8, 3);
        let initial = queue.lock().enqueue(target, pty_size(8, 3), Instant::now());
        queue.lock().dequeue_for_worker();
        let checks = std::cell::Cell::new(0);
        #[cfg(feature = "disruptor-pane-io")]
        let action_ring = ArrayQueue::new(4);
        let (prepared, _) = LocalPane::prepare_resize_reflow(
            &terminal,
            #[cfg(feature = "disruptor-pane-io")]
            &action_ring,
            target,
            || {
                assert!(
                    terminal.try_lock().is_some(),
                    "preparation must release the terminal lock"
                );
                let mut queue = queue
                    .try_lock()
                    .expect("preparation must release admission");
                checks.set(checks.get() + 1);
                if checks.get() == 3 {
                    queue.enqueue(term_size(120, 3), pty_size(120, 3), Instant::now());
                }
                queue
                    .superseded_by(ResizeCancellationToken::new(initial.seq))
                    .is_some()
            },
        );
        assert!(
            prepared.is_none(),
            "a superseded preparation must not be installed"
        );
        assert_eq!(
            checks.get(),
            3,
            "exercise cancellation after source capture"
        );
        let mut terminal = terminal.lock();
        let (decision, _) =
            with_resize_commit_barrier(&queue, ResizeCancellationToken::new(initial.seq), || {
                terminal.resize(target)
            });
        assert!(matches!(decision, ResizeCommitDecision::Superseded { .. }));
        assert_eq!(terminal.get_size(), term_size(80, 3));
    }

    #[test]
    fn resize_commit_barrier_rejects_intent_superseded_before_entry() {
        let queue = Mutex::new(ResizeQueueState::default());
        let first = queue
            .lock()
            .enqueue(term_size(80, 24), pty_size(80, 24), Instant::now());
        queue.lock().dequeue_for_worker();
        let newer = queue
            .lock()
            .enqueue(term_size(120, 40), pty_size(120, 40), Instant::now());
        let committed = AtomicBool::new(false);

        let (decision, _) =
            with_resize_commit_barrier(&queue, ResizeCancellationToken::new(first.seq), || {
                committed.store(true, Ordering::Release)
            });

        assert_eq!(
            decision,
            ResizeCommitDecision::Superseded { by_seq: newer.seq },
        );
        assert!(
            !committed.load(Ordering::Acquire),
            "a target superseded before the barrier must never commit",
        );
    }

    #[test]
    fn resize_commit_barrier_closes_check_to_commit_enqueue_gap() {
        let queue = Arc::new(Mutex::new(ResizeQueueState::default()));
        let initial = queue
            .lock()
            .enqueue(term_size(80, 24), pty_size(80, 24), Instant::now());
        queue.lock().dequeue_for_worker();

        let (attempt_tx, attempt_rx) = sync_channel(0);
        let (probe_tx, probe_rx) = sync_channel(0);
        let (enqueued_tx, enqueued_rx) = sync_channel(0);
        let queue_for_enqueue = Arc::clone(&queue);
        let enqueuer = std::thread::spawn(move || {
            attempt_tx
                .send(())
                .expect("commit barrier probe must start");
            let acquired_during_commit = queue_for_enqueue.try_lock().is_some();
            probe_tx
                .send(acquired_during_commit)
                .expect("commit barrier probe result must be observed");
            let outcome = queue_for_enqueue.lock().enqueue(
                term_size(120, 40),
                pty_size(120, 40),
                Instant::now(),
            );
            enqueued_tx
                .send(outcome)
                .expect("post-commit enqueue result must be observed");
        });

        let committed = AtomicBool::new(false);
        let (decision, _) = with_resize_commit_barrier(
            queue.as_ref(),
            ResizeCancellationToken::new(initial.seq),
            || {
                attempt_rx
                    .recv()
                    .expect("enqueuer must reach the locked commit barrier");
                assert!(
                    !probe_rx
                        .recv()
                        .expect("enqueuer must report whether it crossed the barrier"),
                    "enqueue must not linearize between the final stale check and commit",
                );
                assert_eq!(enqueued_rx.try_recv(), Err(TryRecvError::Empty));
                committed.store(true, Ordering::Release);
            },
        );

        assert_eq!(decision, ResizeCommitDecision::Committed(()));
        assert!(committed.load(Ordering::Acquire));
        let newer = enqueued_rx
            .recv()
            .expect("enqueue must complete after the commit guard is released");
        enqueuer.join().expect("barrier probe thread must finish");
        assert!(newer.seq > initial.seq);
        assert_eq!(
            queue.lock().pending.as_ref().map(|pending| pending.seq),
            Some(newer.seq),
        );
    }
    // Only the ring-specific tests require the optional disruptor feature.
    // Checkpoint and surface-capture regressions below remain in the ordinary
    // test module and share its terminal/PTY fixtures.
    /// ft-87qfi keep-gate: the lock-free SPSC ring's concurrency contract.
    ///
    /// This is the gate that decides whether the disruptor moonshot is safe to keep.
    /// The whole risk of the technique is a lock-free ordering bug, so this exercises
    /// the exact primitive the pane->render staging ring is built on
    /// (`crossbeam::queue::ArrayQueue<Vec<u8>>`, used the same way: producer thread
    /// pushes batches with back-pressure on full, consumer thread drains, spinning on
    /// empty) and asserts EXACT in-order delivery — zero loss, zero duplication, zero
    /// reordering — across many iterations while the small bounded ring repeatedly
    /// fills, wraps, and empties. No `unsafe`.
    #[cfg(feature = "disruptor-pane-io")]
    mod disruptor_ring_keep_gate {
        use super::*;
        use crossbeam::queue::ArrayQueue;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use std::thread;

        /// A batch is the little-endian bytes of its sequence index plus a sentinel
        /// tail byte, so every batch is self-identifying and framing corruption is
        /// detectable. Mirrors the real ring's `Vec<Action>` batches.
        const TAIL_SENTINEL: u8 = 0xAB;
        const BATCH_LEN: usize = 9; // 8 index bytes + 1 sentinel

        fn make_batch(index: u64) -> Vec<u8> {
            let mut batch = index.to_le_bytes().to_vec();
            batch.push(TAIL_SENTINEL);
            batch
        }

        fn decode_index(batch: &[u8]) -> u64 {
            assert_eq!(batch.len(), BATCH_LEN, "batch framing corrupted (len)");
            assert_eq!(
                batch[8], TAIL_SENTINEL,
                "batch framing corrupted (sentinel)"
            );
            let mut idx_bytes = [0u8; 8];
            idx_bytes.copy_from_slice(&batch[..8]);
            u64::from_le_bytes(idx_bytes)
        }

        #[derive(Debug)]
        struct TestTermConfig;

        impl TerminalConfiguration for TestTermConfig {
            fn color_palette(&self) -> ColorPalette {
                ColorPalette::default()
            }
        }

        struct TestMasterPty;

        impl MasterPty for TestMasterPty {
            fn resize(&self, _size: PtySize) -> Result<(), Error> {
                Ok(())
            }

            fn get_size(&self) -> Result<PtySize, Error> {
                Ok(PtySize::default())
            }

            fn try_clone_reader(&self) -> Result<Box<dyn std::io::Read + Send>, Error> {
                Ok(Box::new(std::io::Cursor::new(Vec::new())))
            }

            fn take_writer(&self) -> Result<Box<dyn std::io::Write + Send>, Error> {
                Ok(Box::new(Vec::<u8>::new()))
            }

            #[cfg(unix)]
            fn process_group_leader(&self) -> Option<libc::pid_t> {
                None
            }

            #[cfg(unix)]
            fn as_raw_fd(&self) -> Option<std::os::fd::RawFd> {
                None
            }

            #[cfg(unix)]
            fn tty_name(&self) -> Option<std::path::PathBuf> {
                None
            }
        }

        #[derive(Clone, Debug)]
        struct TestChild;

        impl ChildKiller for TestChild {
            fn kill(&mut self) -> IoResult<()> {
                Ok(())
            }

            fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
                Box::new(self.clone())
            }
        }

        impl Child for TestChild {
            fn try_wait(&mut self) -> IoResult<Option<ExitStatus>> {
                Ok(Some(ExitStatus::with_exit_code(0)))
            }

            fn wait(&mut self) -> IoResult<ExitStatus> {
                Ok(ExitStatus::with_exit_code(0))
            }

            fn process_id(&self) -> Option<u32> {
                None
            }
        }

        fn test_terminal(size: TerminalSize) -> Terminal {
            Terminal::new(
                size,
                Arc::new(TestTermConfig),
                "WezTerm",
                "test",
                Box::new(Vec::new()),
            )
        }

        fn term_size(cols: usize, rows: usize) -> TerminalSize {
            TerminalSize {
                cols,
                rows,
                pixel_width: cols,
                pixel_height: rows,
                dpi: 96,
            }
        }

        fn pty_size(cols: u16, rows: u16) -> PtySize {
            PtySize {
                cols,
                rows,
                pixel_width: cols,
                pixel_height: rows,
            }
        }

        #[test]
        fn resize_worker_drains_staged_actions_before_noop_probe() {
            let size = term_size(10, 1);
            let ring = ArrayQueue::new(4);
            ring.push(vec![Action::Print('x')])
                .expect("ring should accept staged action");

            let terminal = Mutex::new(test_terminal(size));
            let pty: Mutex<Box<dyn MasterPty>> = Mutex::new(Box::new(TestMasterPty));
            let resize_queue = Mutex::new(ResizeQueueState {
                pending: None,
                next_seq: 1,
                worker_running: true,
                last_proven_pty_size: Some(pty_size(10, 1)),
            });
            let metrics = LocalPane::apply_resize_sync(
                7,
                &terminal,
                &Mutex::new(None),
                &ring,
                &pty,
                &resize_queue,
                1,
                size,
                pty_size(10, 1),
                ResizeCancellationToken::new(1),
            )
            .expect("resize probe should succeed");

            assert!(metrics.noop);
            assert!(
                ring.is_empty(),
                "resize worker left staged actions undrained"
            );
            assert_eq!(
                terminal.lock().cursor_pos().x,
                1,
                "staged output must be applied before resize observes terminal state"
            );
        }

        #[test]
        fn checkpoint_lock_drains_disruptor_before_pending_actions_and_model_capture() {
            use crate::guardian_checkpoint::capture_and_bind_live_parser_checkpoint;
            use crate::guardian_output_journal::{
                GuardianOutputCipher, GuardianOutputJournal, GuardianOutputJournalLimits,
                GuardianOutputSegmentIdentity,
            };
            use std::io::Read;

            let size = term_size(10, 1);
            let durable_pane_id = uuid::Uuid::new_v4();
            let segment =
                GuardianOutputSegmentIdentity::new(durable_pane_id, uuid::Uuid::new_v4(), 1, None)
                    .expect("valid checkpoint segment");
            let directory = tempfile::tempdir().expect("private journal directory");
            let directory_file =
                std::fs::File::open(directory.path()).expect("open journal parent");
            rustix::fs::fchmod(&directory_file, rustix::fs::Mode::from_raw_mode(0o700))
                .expect("make journal parent private");
            let mut journal = GuardianOutputJournal::create_new_at(
                &directory_file,
                std::ffi::OsStr::new("checkpoint.segment"),
                segment,
                GuardianOutputCipher::try_from_key_slice(&[0x5a; 32]).expect("valid cipher"),
                GuardianOutputJournalLimits::default(),
            )
            .expect("create checkpoint journal");
            journal
                .sync_parent_directory_and_activate()
                .expect("activate checkpoint journal");
            let receipt = journal.append_and_sync(b"ab").expect("commit parser bytes");

            let pane = Arc::new(LocalPane::new(
                1,
                test_terminal(size),
                Box::new(TestChild),
                Box::new(TestMasterPty),
                Box::new(Vec::<u8>::new()),
                1,
                *durable_pane_id.as_bytes(),
                "checkpoint-disruptor-test".to_string(),
            ));
            let registered_pane: Arc<dyn Pane> = pane.clone();
            let mux = Arc::new(crate::Mux::new(None));
            let generation = crate::PaneRegistrationGeneration::new(
                pane.pane_id(),
                &mux.pane_retirements,
                Arc::downgrade(&mux),
            );
            {
                let _registration = mux.pane_registration.lock();
                mux.insert_pane_registration_locked(
                    pane.pane_id(),
                    pane.domain_id(),
                    &registered_pane,
                    &generation,
                )
                .expect("register checkpoint pane without a competing parser thread");
            }
            let operation = mux
                .capture_pane_operation(pane.pane_id())
                .expect("admit current pane operation");
            let control = &generation.live_parser_checkpoint;
            let (mut writer, mut reader) = crate::allocate_socketpair().expect("parser socket");
            writer
                .set_non_blocking(true)
                .expect("nonblocking parser writer");
            let (mut wake_writer, _wake_reader) =
                crate::allocate_socketpair().expect("wake socket");
            wake_writer
                .set_non_blocking(true)
                .expect("nonblocking wake writer");
            control
                .attach_reader_channels(writer, wake_writer)
                .expect("attach parser channels");
            let target = operation
                .authorize_guardian_output_delivery(segment, receipt, Arc::<[u8]>::from(&b"ab"[..]))
                .expect("authorize the exact journal bytes for this registration");
            control
                .write_delivered_bytes(b"ab")
                .expect("deliver authenticated bytes");
            let mut delivered = [0; 2];
            reader
                .read_exact(&mut delivered)
                .expect("read delivered parser bytes");
            assert_eq!(&delivered, b"ab");
            let mut parser = termwiz::escape::parser::Parser::new();
            let mut staged = Vec::new();
            parser.parse(&delivered[..1], |action| action.append_to(&mut staged));
            let mut pending = Vec::new();
            parser.parse(&delivered[1..], |action| action.append_to(&mut pending));
            assert_eq!(
                control.record_parsed_bytes(delivered.len()).unwrap(),
                target
            );
            let ground = parser
                .recovery_ground_boundary()
                .expect("two printable bytes end at parser ground");
            let limits = TerminalCheckpointLimits::default();
            let (request_id, completion) = control
                .register_checkpoint(
                    &registered_pane,
                    &generation,
                    durable_pane_id,
                    segment,
                    receipt,
                    limits,
                )
                .expect("register checkpoint at the authenticated delivery fence");
            let request = control
                .begin_capture(target)
                .unwrap()
                .expect("admit capture");
            let capture_operation = generation.try_acquire().expect("lease current generation");
            {
                // An idle producer applies immediately. Exercise real contention
                // so capture, rather than the producer, must drain this batch.
                let _terminal = pane.terminal.lock();
                pane.perform_actions(staged);
            }
            assert!(
                !pane.action_ring.is_empty(),
                "fixture must stage its first parser batch in the disruptor"
            );
            let checkpoint = capture_and_bind_live_parser_checkpoint(
                &registered_pane,
                &capture_operation,
                &request,
                &mut pending,
                ground,
            )
            .expect("capture model through the production LocalPane lock path");
            control.complete_capture(request_id, Ok(checkpoint));
            let checkpoint = completion
                .try_recv()
                .unwrap()
                .expect("publish captured model");

            assert!(
                pane.action_ring.is_empty(),
                "checkpoint left disruptor actions staged"
            );
            assert!(
                pending.is_empty(),
                "checkpoint left parser actions unapplied"
            );
            assert_eq!(checkpoint.parser_stream_bytes(), 2);
            assert_eq!(
                pane.terminal.lock().cursor_pos().x,
                2,
                "ring action must precede pending action in captured model"
            );
            assert_eq!(pane.get_lines(0..1).1[0].as_str().trim_end(), "ab");
            assert_eq!(
                checkpoint.terminal_checkpoint().canonical_payload(),
                pane.terminal
                    .lock()
                    .capture_recovery_checkpoint(limits)
                    .unwrap()
                    .canonical_payload(),
                "published checkpoint must contain the complete ordered model"
            );
            control.close_reader_channels();
            drop(capture_operation);
            drop(operation);
            assert!(mux.remove_pane_registration_if_same(pane.pane_id(), &registered_pane));
        }

        #[test]
        fn spsc_ring_delivers_every_batch_exactly_once_in_order() {
            // Small, non-power-of-two capacity so the ring wraps and hits full and
            // empty edges thousands of times per iteration.
            const CAP: usize = 7;
            // Enough batches to wrap the ring ~thousands of times per iteration.
            const BATCHES: u64 = 20_000;
            // Many independent runs to vary producer/consumer interleaving.
            const ITERATIONS: usize = 16;

            for iter in 0..ITERATIONS {
                let ring: Arc<ArrayQueue<Vec<u8>>> = Arc::new(ArrayQueue::new(CAP));
                // Signals that the producer has pushed ALL batches. Lets the consumer
                // terminate (instead of hanging) if a batch were lost: once the
                // producer is done and the ring is empty, no more batches can arrive.
                let producer_done = Arc::new(AtomicBool::new(false));

                let producer = {
                    let ring = Arc::clone(&ring);
                    let producer_done = Arc::clone(&producer_done);
                    thread::spawn(move || {
                        for i in 0..BATCHES {
                            let mut pending = make_batch(i);
                            // Bounded ring => back-pressure: spin until it accepts.
                            loop {
                                match ring.push(pending) {
                                    Ok(()) => break,
                                    Err(returned) => {
                                        pending = returned;
                                        std::hint::spin_loop();
                                    }
                                }
                            }
                        }
                        producer_done.store(true, Ordering::Release);
                    })
                };

                let consumer = {
                    let ring = Arc::clone(&ring);
                    let producer_done = Arc::clone(&producer_done);
                    thread::spawn(move || {
                        let mut drained: Vec<u64> = Vec::with_capacity(BATCHES as usize);
                        loop {
                            match ring.pop() {
                                Some(batch) => drained.push(decode_index(&batch)),
                                None => {
                                    // No item right now. If the producer has finished
                                    // and the ring is empty, there is nothing more
                                    // coming — stop (a short count then proves loss).
                                    if producer_done.load(Ordering::Acquire) && ring.is_empty() {
                                        break;
                                    }
                                    std::hint::spin_loop();
                                }
                            }
                        }
                        drained
                    })
                };

                producer.join().expect("producer thread panicked");
                let drained = consumer.join().expect("consumer thread panicked");

                // Zero loss + zero duplication: exactly BATCHES items delivered.
                assert_eq!(
                    drained.len() as u64,
                    BATCHES,
                    "iter {iter}: delivered {} batches, expected {BATCHES} (loss or duplication)",
                    drained.len()
                );
                // Zero reordering: the batch drained at position p is exactly the
                // batch produced at position p. Combined with the exact count above,
                // this proves the drained sequence equals the produced sequence byte
                // for byte, in order.
                for (pos, &index) in drained.iter().enumerate() {
                    assert_eq!(
                        index, pos as u64,
                        "iter {iter}: ordering/identity violation at position {pos}: \
                     got batch index {index} (lock-free SPSC loss/dup/reorder)"
                    );
                }
                // The ring must be fully drained at the end.
                assert!(
                    ring.pop().is_none(),
                    "iter {}: ring not empty after consuming all batches",
                    iter
                );
            }
        }
    }

    struct MuxCheckpointTestSink {
        identity: std::sync::Mutex<frankenterm_term::config::ScrollbackIntervalIdentity>,
        rows: std::sync::Mutex<Vec<(StableRowIndex, Line)>>,
        generation: std::sync::Mutex<frankenterm_term::config::ScrollbackSnapshotGeneration>,
        mutate_on_snapshot: std::sync::atomic::AtomicBool,
        snapshot_probe: std::sync::Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
        deferred: std::sync::atomic::AtomicBool,
        flushed_rows: AtomicUsize,
        pending_capacity: AtomicUsize,
    }

    impl std::fmt::Debug for MuxCheckpointTestSink {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("MuxCheckpointTestSink")
                .finish_non_exhaustive()
        }
    }

    impl MuxCheckpointTestSink {
        fn new() -> Self {
            Self {
                identity: std::sync::Mutex::new(
                    frankenterm_term::config::ScrollbackIntervalIdentity::default(),
                ),
                rows: std::sync::Mutex::new(Vec::new()),
                generation: std::sync::Mutex::new(
                    frankenterm_term::config::ScrollbackSnapshotGeneration::new([1; 16], 1),
                ),
                mutate_on_snapshot: std::sync::atomic::AtomicBool::new(false),
                snapshot_probe: std::sync::Mutex::new(None),
                deferred: std::sync::atomic::AtomicBool::new(false),
                flushed_rows: AtomicUsize::new(0),
                pending_capacity: AtomicUsize::new(usize::MAX),
            }
        }

        fn advance_identity(&self) {
            *self.identity.lock().unwrap() =
                frankenterm_term::config::ScrollbackIntervalIdentity::default();
        }
    }

    impl frankenterm_term::config::ScrollbackSpillSink for MuxCheckpointTestSink {
        fn requires_scrollback_flush(&self) -> bool {
            self.deferred.load(Ordering::SeqCst)
        }

        fn flush_scrollback(&self) -> Result<(), frankenterm_term::config::ScrollbackSpillError> {
            self.flushed_rows
                .store(self.rows.lock().unwrap().len(), Ordering::SeqCst);
            Ok(())
        }

        fn clear_scrollback(
            &self,
        ) -> Result<
            frankenterm_term::config::ScrollbackClearCommit,
            frankenterm_term::config::ScrollbackSpillError,
        > {
            panic!("checkpoint capture must not clear its source scrollback")
        }

        fn store_scrollback_line(
            &self,
            stable_row: StableRowIndex,
            line: &Line,
            _max_retained_rows: usize,
        ) -> bool {
            let mut rows = self.rows.lock().unwrap();
            if rows
                .len()
                .saturating_sub(self.flushed_rows.load(Ordering::SeqCst))
                >= self.pending_capacity.load(Ordering::SeqCst)
            {
                return false;
            }
            rows.push((stable_row, line.clone()));
            true
        }

        fn load_scrollback_line(&self, stable_row: StableRowIndex) -> Option<Line> {
            self.rows
                .lock()
                .unwrap()
                .iter()
                .find(|(r, _)| *r == stable_row)
                .map(|(_, l)| l.clone())
        }

        fn oldest_scrollback_row(&self) -> Option<StableRowIndex> {
            self.rows.lock().unwrap().first().map(|(r, _)| *r)
        }

        fn retained_scrollback_rows(&self) -> usize {
            self.rows.lock().unwrap().len()
        }

        fn retained_scrollback_bytes(&self) -> usize {
            self.rows.lock().unwrap().len() * 80
        }

        fn try_capture_scrollback_usage(&self) -> frankenterm_term::config::ScrollbackUsageCapture {
            let Ok(rows) = self.rows.try_lock() else {
                return frankenterm_term::config::ScrollbackUsageCapture::Busy;
            };
            let count = rows.len();
            frankenterm_term::config::ScrollbackUsageCapture::Ready(
                frankenterm_term::config::ScrollbackUsage {
                    rows: count,
                    bytes: count * 80,
                },
            )
        }

        fn try_capture_scrollback_interval(
            &self,
        ) -> frankenterm_term::config::ScrollbackIntervalCapture {
            let rows = self.rows.lock().unwrap();
            let range = if rows.is_empty() {
                None
            } else {
                let first = rows.first().unwrap().0;
                let last = rows.last().unwrap().0;
                Some(first..last + 1)
            };
            self.identity.lock().unwrap().capture(range)
        }

        fn snapshot_scrollback(
            &self,
            expected_newest_exclusive: StableRowIndex,
            _limits: frankenterm_term::config::ScrollbackSnapshotLimits,
        ) -> Result<
            frankenterm_term::config::ScrollbackSnapshot,
            frankenterm_term::config::ScrollbackSpillError,
        > {
            if let Some(probe) = self.snapshot_probe.lock().unwrap().as_ref() {
                probe();
            }
            if self
                .mutate_on_snapshot
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                self.advance_identity();
            }
            let rows = self.rows.lock().unwrap();
            let filtered: Vec<Line> = rows
                .iter()
                .filter(|(idx, _)| *idx < expected_newest_exclusive)
                .map(|(_, line)| line.clone())
                .collect();
            let oldest = if filtered.is_empty() {
                None
            } else {
                filtered
                    .first()
                    .and_then(|_| rows.first().map(|(idx, _)| *idx))
            };
            let gen = *self.generation.lock().unwrap();
            let count = filtered.len();
            frankenterm_term::config::ScrollbackSnapshot::from_contiguous_rows(
                gen,
                frankenterm_term::config::ScrollbackSnapshotFidelity::ExactSemantic,
                oldest,
                expected_newest_exclusive,
                (count * 80) as u64,
                count * 80,
                filtered,
            )
        }

        fn replace_scrollback_prefix(
            &self,
            _expected_generation: Option<frankenterm_term::config::ScrollbackSnapshotGeneration>,
            _prefix: frankenterm_term::config::ScrollbackPrefix<'_>,
            _max_retained_rows: usize,
        ) -> Result<
            frankenterm_term::config::ScrollbackReplaceCommit,
            frankenterm_term::config::ScrollbackSpillError,
        > {
            Err(frankenterm_term::config::ScrollbackSpillError::StorageUnavailable)
        }
    }

    #[derive(Debug)]
    struct MuxCheckpointTestConfig {
        sink: Arc<MuxCheckpointTestSink>,
    }

    impl TerminalConfiguration for MuxCheckpointTestConfig {
        fn color_palette(&self) -> ColorPalette {
            ColorPalette::default()
        }

        fn scrollback_size(&self) -> usize {
            1000
        }

        fn scrollback_tier_config(&self) -> frankenterm_term::config::ScrollbackTierConfig {
            frankenterm_term::config::ScrollbackTierConfig {
                enabled: true,
                hot_lines: 1,
                warm_max_bytes: 0,
            }
        }

        fn scrollback_spill_sink(
            &self,
        ) -> Option<Arc<dyn frankenterm_term::config::ScrollbackSpillSink>> {
            Some(Arc::clone(&self.sink) as Arc<dyn frankenterm_term::config::ScrollbackSpillSink>)
        }
    }

    fn make_legacy_test_pane(pane_id: PaneId, terminal: Terminal) -> LocalPane {
        LocalPane::new(
            pane_id,
            terminal,
            Box::new(KillCountingChild {
                kills: Arc::new(AtomicUsize::new(0)),
            }),
            Box::new(GuardianLifetimeTestMasterPty),
            Box::new(Vec::<u8>::new()),
            1,
            [0x88; 16],
            "legacy-test-pane".to_string(),
        )
    }

    #[test]
    fn deferred_scrollback_flushes_when_overflow_settles_between_slices() {
        #[derive(Debug)]
        struct SettlingConfig(Arc<MuxCheckpointTestSink>);

        impl TerminalConfiguration for SettlingConfig {
            fn color_palette(&self) -> ColorPalette {
                ColorPalette::default()
            }

            fn scrollback_size(&self) -> usize {
                2048
            }

            fn scrollback_tier_config(&self) -> frankenterm_term::config::ScrollbackTierConfig {
                // Model a configuration change at the slice boundary without
                // scheduler timing: the first slice admits 16 rows, then the
                // new hot budget accommodates every remaining resident row.
                let settled = !self.0.deferred.load(Ordering::SeqCst)
                    || self.0.rows.lock().unwrap().len() >= 16;
                frankenterm_term::config::ScrollbackTierConfig {
                    enabled: true,
                    hot_lines: if settled { 1000 } else { 1 },
                    warm_max_bytes: 0,
                }
            }

            fn scrollback_spill_sink(
                &self,
            ) -> Option<Arc<dyn frankenterm_term::config::ScrollbackSpillSink>> {
                Some(self.0.clone())
            }
        }

        let sink = Arc::new(MuxCheckpointTestSink::new());
        let mut terminal = Terminal::new(
            term_size(80, 4),
            Arc::new(SettlingConfig(sink.clone())),
            "FrankenTerm",
            "deferred-settlement-test",
            Box::new(Vec::<u8>::new()),
        );
        for row in 0..50 {
            terminal.advance_bytes(format!("row-{row:02}\r\n").as_bytes());
        }
        assert!(sink.rows.lock().unwrap().is_empty());
        sink.deferred.store(true, Ordering::SeqCst);
        let pane = make_legacy_test_pane(786, terminal);
        pane.drain_scrollback_outside_terminal(sink.clone());
        let rows = sink.rows.lock().unwrap();
        assert_eq!(rows.len(), 16, "one geometry slice must have been queued");
        assert_eq!(sink.flushed_rows.load(Ordering::SeqCst), rows.len());
        for (index, (stable_row, line)) in rows.iter().enumerate() {
            assert_eq!(*stable_row, index as StableRowIndex);
            assert_eq!(line.as_str().trim_end(), format!("row-{index:02}"));
        }
    }

    #[test]
    fn deferred_scrollback_flushes_full_yielded_batches_without_failure_backoff() {
        use metrics::{Counter, Gauge, Histogram, Key, KeyName, Metadata, SharedString, Unit};

        struct NoFailureBackoff(AtomicUsize);
        impl metrics::Recorder for NoFailureBackoff {
            fn describe_counter(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
            fn describe_gauge(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
            fn describe_histogram(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
            fn register_counter(&self, key: &Key, _: &Metadata<'_>) -> Counter {
                assert_ne!(key.name(), "mux.scrollback.persistence_backpressure");
                if key.name() == "mux.scrollback.geometry_yields" {
                    self.0.fetch_add(1, Ordering::SeqCst);
                }
                Counter::noop()
            }
            fn register_gauge(&self, _: &Key, _: &Metadata<'_>) -> Gauge {
                Gauge::noop()
            }
            fn register_histogram(&self, _: &Key, _: &Metadata<'_>) -> Histogram {
                Histogram::noop()
            }
        }

        #[derive(Debug)]
        struct BoundedConfig(Arc<MuxCheckpointTestSink>);
        impl TerminalConfiguration for BoundedConfig {
            fn color_palette(&self) -> ColorPalette {
                ColorPalette::default()
            }
            fn scrollback_size(&self) -> usize {
                2048
            }
            fn scrollback_tier_config(&self) -> frankenterm_term::config::ScrollbackTierConfig {
                frankenterm_term::config::ScrollbackTierConfig {
                    enabled: true,
                    hot_lines: if self.0.deferred.load(Ordering::SeqCst) {
                        1
                    } else {
                        1000
                    },
                    warm_max_bytes: 0,
                }
            }
            fn scrollback_spill_sink(
                &self,
            ) -> Option<Arc<dyn frankenterm_term::config::ScrollbackSpillSink>> {
                Some(self.0.clone())
            }
        }

        let sink = Arc::new(MuxCheckpointTestSink::new());
        let mut terminal = Terminal::new(
            term_size(80, 4),
            Arc::new(BoundedConfig(sink.clone())),
            "FrankenTerm",
            "deferred-full-batch-test",
            Box::new(Vec::<u8>::new()),
        );
        for row in 0..70 {
            terminal.advance_bytes(format!("row-{row:02}\r\n").as_bytes());
        }
        assert!(sink.rows.lock().unwrap().is_empty());
        sink.pending_capacity.store(16, Ordering::SeqCst);
        sink.deferred.store(true, Ordering::SeqCst);
        let pane = make_legacy_test_pane(787, terminal);
        let recorder = NoFailureBackoff(AtomicUsize::new(0));
        // Thread-local recording observes the real backoff branch without a
        // wall-clock assertion or interference from parallel tests.
        metrics::with_local_recorder(&recorder, || {
            pane.drain_scrollback_outside_terminal(sink.clone());
        });
        assert!(recorder.0.load(Ordering::SeqCst) >= 4);
        let rows = sink.rows.lock().unwrap();
        assert_eq!(rows.len(), 66);
        assert_eq!(sink.flushed_rows.load(Ordering::SeqCst), rows.len());
        for (index, (stable_row, line)) in rows.iter().enumerate() {
            assert_eq!(*stable_row, index as StableRowIndex);
            assert_eq!(line.as_str().trim_end(), format!("row-{index:02}"));
        }
    }

    fn seed_checkpoint_cold_rows(terminal: &mut Terminal, rows: &[&str]) {
        for row in rows {
            terminal.advance_bytes(row.as_bytes());
            terminal.advance_bytes(b"\r\n");
        }
        // Scroll past the one retained hot history row through the actual
        // parser and spill path, then position visible output at the top.
        terminal.advance_bytes(b"\x1b[24;1H");
        for _ in 0..=rows.len() {
            terminal.advance_bytes(b"\r\n");
        }
        terminal.advance_bytes(b"\x1b[1;1H");
    }

    #[test]
    fn test_legacy_terminal_checkpoint_positive_roundtrip() {
        let sink = Arc::new(MuxCheckpointTestSink::new());

        let config: Arc<dyn TerminalConfiguration> = Arc::new(MuxCheckpointTestConfig {
            sink: Arc::clone(&sink),
        });

        let mut terminal = Terminal::new(
            term_size(80, 24),
            config,
            "FrankenTerm",
            "checkpoint-pos-test",
            Box::new(Vec::<u8>::new()),
        );

        seed_checkpoint_cold_rows(
            &mut terminal,
            &["cold scrollback line 0", "cold scrollback line 1"],
        );
        assert_eq!(sink.retained_scrollback_rows(), 2);

        terminal.advance_bytes("\x1b[1;31mPrimary \u{1F980} Crab\x1b[0m\r\n".as_bytes());
        terminal.advance_bytes(b"\x1b[?2004h");
        terminal.advance_bytes(b"\x1b[6;11H");

        terminal
            .advance_bytes("\x1b[?1049h\x1b[4;32mAlternate \u{26A1} HighVolt\x1b[0m".as_bytes());

        let pane = make_legacy_test_pane(777, terminal);

        let authority = ModelParserCaptureAuthority::issue();
        let limits = TerminalCheckpointLimits::default();
        let mut pending = Vec::new();
        let parser = termwiz::escape::parser::Parser::new();
        let ground = parser
            .recovery_ground_boundary()
            .expect("fresh parser must be at recovery ground");

        let recovery = pane
            .capture_legacy_terminal_checkpoint(authority, &mut pending, ground, limits)
            .expect("legacy terminal checkpoint capture must succeed");

        assert_eq!(recovery.rows(), 24);
        assert_eq!(recovery.cols(), 80);
        assert!(!recovery.canonical_payload().is_empty());

        let validated =
            TerminalCheckpointV2::decode_canonical_json(recovery.canonical_payload(), limits)
                .expect("canonical payload must decode and validate");

        let checkpoint = validated.checkpoint();

        // 1. Both screens preserved
        assert_eq!(checkpoint.primary_lines_count(), 27);
        assert_eq!(checkpoint.alternate_lines_count(), 24);
        assert!(checkpoint.is_alternate_screen_active());

        // 2. Scrollback preserved
        assert_eq!(checkpoint.primary_cold_prefix_lines(), 2);
        assert_eq!(checkpoint.primary_cell_text(0, 0), Some("c"));
        assert_eq!(checkpoint.primary_cell_text(0, 1), Some("o"));

        // 3. Modes preserved (bracketed paste)
        assert!(checkpoint.bracketed_paste());

        // 4. Cursor preserved on active alternate screen
        let (cursor_row, _cursor_col) = checkpoint.cursor_position();
        assert_eq!(cursor_row, 0);

        // 5. Styled cells & Unicode preserved on alternate screen
        assert_eq!(checkpoint.alternate_cell_text(0, 0), Some("A"));
        assert_eq!(
            checkpoint.alternate_cell_underline(0, 0),
            Some(termwiz::cell::Underline::Single)
        );
        assert_eq!(checkpoint.alternate_cell_text(0, 10), Some("\u{26A1}"));

        // 6. Styled cells & Unicode preserved on primary screen
        assert_eq!(checkpoint.primary_cell_text(3, 0), Some("P"));
        assert_eq!(
            checkpoint.primary_cell_intensity(3, 0),
            Some(termwiz::cell::Intensity::Bold)
        );
        assert_eq!(checkpoint.primary_cell_text(3, 8), Some("\u{1F980}"));
    }

    #[test]
    fn test_legacy_terminal_checkpoint_rejects_pending_actions() {
        let terminal = guardian_lifetime_test_terminal();
        let pane = make_legacy_test_pane(778, terminal);

        let authority = ModelParserCaptureAuthority::issue();
        let limits = TerminalCheckpointLimits::default();
        let mut pending = vec![Action::Print('h'), Action::Print('i')];
        let parser = termwiz::escape::parser::Parser::new();
        let ground = parser
            .recovery_ground_boundary()
            .expect("fresh parser must be at recovery ground");

        let result = pane.capture_legacy_terminal_checkpoint_with_policy(
            authority,
            &mut pending,
            ground,
            limits,
            PendingActionDrainPolicy::RequireEmpty,
        );

        match result {
            Err(LegacyTerminalCaptureError::PendingActionsRemain(count)) => {
                assert_eq!(count, 2);
            }
            other => panic!("expected PendingActionsRemain(2), got {:?}", other),
        }

        // Under DrainAndApply policy, pending actions are applied and drained
        let ground = parser
            .recovery_ground_boundary()
            .expect("parser remains at recovery ground after the refused capture");
        let result = pane.capture_legacy_terminal_checkpoint_with_policy(
            authority,
            &mut pending,
            ground,
            limits,
            PendingActionDrainPolicy::DrainAndApply,
        );
        assert!(result.is_ok());
        assert!(pending.is_empty());
    }

    #[test]
    fn test_legacy_terminal_checkpoint_rejects_false_guardian_authority() {
        let identity = guardian_lifetime_test_identity(1);
        let control = Arc::new(FencedGuardianLeaseControl::new(identity));
        let kills = Arc::new(AtomicUsize::new(0));
        let pane = guardian_lifetime_test_pane(779, identity, control, kills);

        let authority = ModelParserCaptureAuthority::issue();
        let limits = TerminalCheckpointLimits::default();
        let mut pending = Vec::new();
        let parser = termwiz::escape::parser::Parser::new();
        let ground = parser
            .recovery_ground_boundary()
            .expect("fresh parser must be at recovery ground");

        let result =
            pane.capture_legacy_terminal_checkpoint(authority, &mut pending, ground, limits);

        match result {
            Err(LegacyTerminalCaptureError::FalseGuardianAuthority) => {}
            other => panic!("expected FalseGuardianAuthority, got {:?}", other),
        }
    }

    #[test]
    fn test_legacy_terminal_checkpoint_rejects_stale_cold_generation() {
        let sink = Arc::new(MuxCheckpointTestSink::new());
        sink.mutate_on_snapshot
            .store(true, std::sync::atomic::Ordering::SeqCst);

        let config: Arc<dyn TerminalConfiguration> = Arc::new(MuxCheckpointTestConfig {
            sink: Arc::clone(&sink),
        });

        let mut terminal = Terminal::new(
            term_size(80, 24),
            config,
            "FrankenTerm",
            "stale-cold-test",
            Box::new(Vec::<u8>::new()),
        );
        seed_checkpoint_cold_rows(&mut terminal, &["cold row 0"]);
        assert_eq!(sink.retained_scrollback_rows(), 1);

        let pane = make_legacy_test_pane(780, terminal);

        let authority = ModelParserCaptureAuthority::issue();
        let limits = TerminalCheckpointLimits::default();
        let mut pending = Vec::new();
        let parser = termwiz::escape::parser::Parser::new();
        let ground = parser
            .recovery_ground_boundary()
            .expect("fresh parser must be at recovery ground");

        let result =
            pane.capture_legacy_terminal_checkpoint(authority, &mut pending, ground, limits);

        match result {
            Err(LegacyTerminalCaptureError::StaleColdGeneration) => {}
            other => panic!("expected StaleColdGeneration, got {:?}", other),
        }
    }

    #[test]
    fn test_legacy_terminal_checkpoint_lock_release_causal() {
        let sink = Arc::new(MuxCheckpointTestSink::new());

        let config: Arc<dyn TerminalConfiguration> = Arc::new(MuxCheckpointTestConfig {
            sink: Arc::clone(&sink),
        });

        let mut terminal = Terminal::new(
            term_size(80, 24),
            config,
            "FrankenTerm",
            "causal-lock-test",
            Box::new(Vec::<u8>::new()),
        );
        seed_checkpoint_cold_rows(&mut terminal, &["cold row 0"]);
        assert_eq!(sink.retained_scrollback_rows(), 1);

        let pane = Arc::new(make_legacy_test_pane(781, terminal));

        // Install a probe into the spill sink that executes during `snapshot_scrollback`.
        // If the terminal mutex is dropped before cold materialization occurs,
        // `pane.terminal.try_lock()` will succeed (return Some) inside the probe!
        let pane_weak = Arc::downgrade(&pane);
        let lock_acquired_during_snapshot = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let probe_flag = Arc::clone(&lock_acquired_during_snapshot);
        *sink.snapshot_probe.lock().unwrap() = Some(Arc::new(move || {
            let pane = pane_weak.upgrade().expect("capture keeps pane alive");
            if pane.terminal.try_lock().is_some() && pane.output_application.try_lock().is_some() {
                probe_flag.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }));

        let authority = ModelParserCaptureAuthority::issue();
        let limits = TerminalCheckpointLimits::default();
        let mut pending = Vec::new();
        let parser = termwiz::escape::parser::Parser::new();
        let ground = parser
            .recovery_ground_boundary()
            .expect("fresh parser must be at recovery ground");

        let result =
            pane.capture_legacy_terminal_checkpoint(authority, &mut pending, ground, limits);

        assert!(result.is_ok(), "capture must succeed: {:?}", result.err());
        assert!(
            lock_acquired_during_snapshot.load(std::sync::atomic::Ordering::SeqCst),
            "terminal and output-application mutexes must be free during cold materialization"
        );
    }

    #[test]
    fn test_legacy_terminal_checkpoint_cold_reflow_roundtrip() {
        let sink = Arc::new(MuxCheckpointTestSink::new());
        let blank_attr = termwiz::cell::CellAttributes::blank();

        let config: Arc<dyn TerminalConfiguration> = Arc::new(MuxCheckpointTestConfig {
            sink: Arc::clone(&sink),
        });

        let mut terminal = Terminal::new(
            term_size(80, 24),
            config,
            "FrankenTerm",
            "cold-reflow-test",
            Box::new(Vec::<u8>::new()),
        );
        seed_checkpoint_cold_rows(&mut terminal, &["unreflowed cold row 0"]);
        assert_eq!(sink.retained_scrollback_rows(), 1);

        let frankenterm_term::config::ScrollbackIntervalCapture::Ready(interval) =
            sink.try_capture_scrollback_interval()
        else {
            panic!("expected sink interval to be ready");
        };

        // Install cold row fragments representing a reflowed split row
        let reflowed_line =
            Line::from_text("reflowed replacement cold row 0", &blank_attr, 1, None);
        let mut replacements = std::collections::BTreeMap::new();
        replacements.insert(0, reflowed_line);

        terminal.set_cold_row_fragments_for_test(
            Arc::clone(&sink) as Arc<dyn frankenterm_term::config::ScrollbackSpillSink>,
            interval,
            replacements,
        );

        let pane = make_legacy_test_pane(782, terminal);

        let authority = ModelParserCaptureAuthority::issue();
        let limits = TerminalCheckpointLimits::default();
        let mut pending = Vec::new();
        let parser = termwiz::escape::parser::Parser::new();
        let ground = parser
            .recovery_ground_boundary()
            .expect("fresh parser must be at recovery ground");

        let recovery = pane
            .capture_legacy_terminal_checkpoint(authority, &mut pending, ground, limits)
            .expect("capture with cold reflow fragments must succeed");

        let validated =
            TerminalCheckpointV2::decode_canonical_json(recovery.canonical_payload(), limits)
                .expect("canonical payload must decode and validate");

        let checkpoint = validated.checkpoint();

        // Verify that the replacement from cold_row_fragments was applied
        assert_eq!(checkpoint.primary_lines_count(), 26);
        assert_eq!(checkpoint.primary_cold_prefix_lines(), 1);
        // Cell (0, 0) should be 'r' from "reflowed", not 'u' from "unreflowed"
        assert_eq!(checkpoint.primary_cell_text(0, 0), Some("r"));
        assert_eq!(checkpoint.primary_cell_text(0, 1), Some("e"));
        assert_eq!(checkpoint.primary_cell_text(0, 2), Some("f"));
    }

    #[test]
    fn capture_surface_snapshot_busy_refusal() {
        let terminal = guardian_lifetime_test_terminal();
        let pane = make_legacy_test_pane(783, terminal);

        // 1. Terminal lock contention
        {
            let _guard = pane.terminal.lock();
            let result = pane.capture_surface_snapshot(0);
            assert!(
                matches!(
                    result,
                    Some(Err(frankenterm_term::screen::ColdReadMetadataBusy))
                ),
                "terminal lock contention must return Some(Err(ColdReadMetadataBusy))"
            );
        }

        // 2. Tmux domain lock contention
        {
            let _guard = pane.tmux_domain.lock();
            let result = pane.capture_surface_snapshot(0);
            assert!(
                matches!(
                    result,
                    Some(Err(frankenterm_term::screen::ColdReadMetadataBusy))
                ),
                "tmux domain lock contention must return Some(Err(ColdReadMetadataBusy))"
            );
        }

        // 3. Line layout observation lock contention
        {
            let _guard = pane.line_layout_observation.lock();
            let result = pane.capture_surface_snapshot(0);
            assert!(
                matches!(
                    result,
                    Some(Err(frankenterm_term::screen::ColdReadMetadataBusy))
                ),
                "line layout observation lock contention must return Some(Err(ColdReadMetadataBusy))"
            );
        }

        // 4. Cold sink busy refusal
        {
            let (cold_pane, _mux, _reg, sink, _token) = cold_resize_fixture(false);
            // Layout admission still succeeds; only the subsequent usage
            // observation is contended, reproducing the gap between probes.
            sink.usage_busy.store(true, Ordering::Release);
            assert!(matches!(cold_pane.get_line_layout(), Ok(Some(_))));
            let result = cold_pane.capture_surface_snapshot(0);
            assert!(
                matches!(
                    result,
                    Some(Err(frankenterm_term::screen::ColdReadMetadataBusy))
                ),
                "cold sink busy must return Some(Err(ColdReadMetadataBusy))"
            );
            sink.usage_busy.store(false, Ordering::Release);
            let result = cold_pane.capture_surface_snapshot(0);
            assert!(result.is_some() && result.as_ref().unwrap().is_ok());
            let snap = result.unwrap().unwrap();
            let status = snap.tiered_scrollback_status.expect("tiered status ready");
            assert_eq!(
                status.cold_sink_retained_lines,
                sink.retained_scrollback_rows()
            );
            sink.usage_unsupported.store(true, Ordering::Release);
            let unsupported = cold_pane.capture_surface_snapshot(0).unwrap().unwrap();
            assert!(unsupported.tiered_scrollback_status.is_none());
            assert_eq!(unsupported.viewport_lines, snap.viewport_lines);
            assert!(sink.retained_scrollback_rows() > 0);
        }

        // 5. Uncontended call succeeds
        let result = pane.capture_surface_snapshot(0);
        assert!(result.is_some() && result.as_ref().unwrap().is_ok());
    }

    #[test]
    fn capture_surface_snapshot_exact_resident_metadata_dirty_and_rows() {
        let mut terminal = guardian_lifetime_test_terminal();
        terminal.advance_bytes(b"hello world\r\nsecond line");
        let pane = make_legacy_test_pane(784, terminal);

        let snap1 = pane
            .capture_surface_snapshot(0)
            .expect("supported backend returns Some")
            .expect("uncontended snapshot succeeds");

        assert_eq!(snap1.dimensions.cols, 80);
        assert_eq!(snap1.dimensions.viewport_rows, 24);
        assert_eq!(snap1.source_sequence, pane.get_current_seqno());
        assert!(!snap1.mouse_grabbed);
        assert!(!snap1.alt_screen_active);
        assert_eq!(snap1.cursor_position, pane.get_cursor_position());
        assert_eq!(snap1.title, pane.get_title());
        assert_eq!(
            snap1.working_dir,
            pane.get_current_working_dir(CachePolicy::AllowStale)
        );

        // Dirty lines for baseline 0 should contain rows 0 and 1
        assert!(snap1.dirty_lines.contains(0));
        assert!(snap1.dirty_lines.contains(1));

        // Viewport lines should have physical_top as start row and 24 rows
        assert_eq!(snap1.viewport_lines.0, snap1.dimensions.physical_top);
        assert_eq!(snap1.viewport_lines.1.len(), 24);
        assert!(snap1.viewport_lines.1[0]
            .as_str()
            .starts_with("hello world"));
        assert!(snap1.viewport_lines.1[1]
            .as_str()
            .starts_with("second line"));

        // Cursor lines should be at cursor_position.y
        assert_eq!(snap1.cursor_lines.0, snap1.cursor_position.y);
        assert_eq!(snap1.cursor_lines.1.len(), 1);

        // Snapshot with baseline = current source sequence: no dirty lines
        let snap2 = pane
            .capture_surface_snapshot(snap1.source_sequence)
            .unwrap()
            .unwrap();
        assert!(snap2.dirty_lines.is_empty());

        // Advance output and verify only new rows are dirty
        pane.terminal.lock().advance_bytes(b"\r\nthird line");
        let snap3 = pane
            .capture_surface_snapshot(snap1.source_sequence)
            .unwrap()
            .unwrap();
        assert!(snap3.dirty_lines.contains(2));
        assert!(!snap3.dirty_lines.contains(0));
        assert_eq!(snap3.viewport_lines.1[2].as_str().trim_end(), "third line");
    }

    #[test]
    fn capture_surface_snapshot_zero_cold_storage_reads() {
        let (pane, _mux, _reg, sink, _token) = cold_resize_fixture(false);

        // Verify fixture has cold rows stored in the sink
        assert!(sink.retained_scrollback_rows() > 0);
        let initial_reads = sink.payload_reads.load(Ordering::Relaxed);
        assert_eq!(
            initial_reads, 0,
            "test sink must start with zero payload reads"
        );

        let snap = pane
            .capture_surface_snapshot(0)
            .expect("local pane supports capture_surface_snapshot")
            .expect("capture succeeds");

        // Viewport lines must capture resident rows only
        assert_eq!(snap.dimensions.viewport_rows, 2);
        assert_eq!(snap.viewport_lines.1.len(), 2);
        assert_eq!(
            snap.tiered_scrollback_status
                .as_ref()
                .expect("tiered status")
                .cold_sink_retained_lines,
            sink.retained_scrollback_rows()
        );

        // Zero cold storage payload reads must have been attempted
        let reads_after = sink.payload_reads.load(Ordering::Relaxed);
        assert_eq!(
            reads_after, 0,
            "capture_surface_snapshot must perform zero cold storage reads"
        );
    }
}
