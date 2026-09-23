use crate::domain::DomainId;
use crate::guardian_checkpoint::{
    LiveParserCaptureAuthority, LiveParserCheckpointAck, LiveParserPaneCaptureError,
};
#[cfg(test)]
use crate::pane::GuardianLiveOutputDelivery;
use crate::pane::{
    CachePolicy, CloseReason, ForEachPaneLogicalLine, GuardianLiveCheckpointPublisher,
    GuardianLiveOutputReader, LogicalLine, Pane, PaneActionAdmissionError,
    PaneActionAdmissionRefusal, PaneId, PaneSurfaceSnapshot, PaneTitleMetadata, Pattern,
    SearchResult, WithPaneLines,
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
use frankenterm_term::terminalstate::checkpoint::TerminalCheckpointV3;
use frankenterm_term::terminalstate::checkpoint::{
    TerminalCheckpointError, TerminalCheckpointLimits,
};
use frankenterm_term::{
    Alert, AlertHandler, Clipboard, DownloadHandler, KeyCode, KeyModifiers, MouseEvent, Progress,
    RecoveryTerminalCheckpointError, RecoveryTerminalCheckpointV3, SemanticZone, StableRowIndex,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
enum MetadataRefusalStage {
    CaptureTerminal,
    CaptureScreen,
    PublishTerminal,
    PublishScreen,
    LayoutTerminal,
    PublishLayoutTerminal,
    LayoutObservation,
    SinkInterval,
    SurfaceTerminal,
    SurfaceTmux,
    LayoutUnavailable,
    DimensionsUnavailable,
    SinkUsage,
    SurfaceGeometry,
    PublicationGeometry,
}

const METADATA_REFUSAL_STAGES: usize = 15;
const METADATA_REFUSAL_LOG_LIMIT: usize = 64;
static METADATA_REFUSALS: [AtomicUsize; METADATA_REFUSAL_STAGES] =
    [const { AtomicUsize::new(0) }; METADATA_REFUSAL_STAGES];
static METADATA_REFUSAL_LOGS: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
std::thread_local! {
    static LAST_METADATA_REFUSAL: std::cell::Cell<Option<MetadataRefusalStage>> = const {
        std::cell::Cell::new(None)
    };
    static COLD_VIEWPORT_AFTER_PUBLISH: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const {
        std::cell::RefCell::new(None)
    };
}

fn record_metadata_refusal(stage: MetadataRefusalStage) {
    #[cfg(test)]
    LAST_METADATA_REFUSAL.with(|last| last.set(Some(stage)));
    if log::log_enabled!(target: "mux::metadata_refusal", log::Level::Debug) {
        let _ = METADATA_REFUSALS[stage as usize].try_update(
            Ordering::Relaxed,
            Ordering::Relaxed,
            |count| Some(count.saturating_add(1)),
        );
    }
}

fn metadata_busy(stage: MetadataRefusalStage) -> frankenterm_term::screen::ColdReadMetadataBusy {
    record_metadata_refusal(stage);
    frankenterm_term::screen::ColdReadMetadataBusy
}

fn reserve_metadata_refusal_log(emitted: &AtomicUsize) -> bool {
    emitted
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
            (count < METADATA_REFUSAL_LOG_LIMIT).then_some(count.saturating_add(1))
        })
        .is_ok()
}

/// Declare before any method-local lock guards, so logging runs after they
/// drop, including early-return paths. No recorder, thread, payload or wire
/// change: opt in with RUST_LOG=mux::metadata_refusal=debug. At most 64 fixed
/// cumulative summaries are emitted per process; these are sampled totals,
/// not a promise of final totals after the emission budget is exhausted.
struct MetadataRefusalDiagnostic(Option<[usize; METADATA_REFUSAL_STAGES]>);

impl MetadataRefusalDiagnostic {
    fn new() -> Self {
        Self(
            log::log_enabled!(target: "mux::metadata_refusal", log::Level::Debug).then(|| {
                std::array::from_fn(|index| METADATA_REFUSALS[index].load(Ordering::Relaxed))
            }),
        )
    }
}

impl Drop for MetadataRefusalDiagnostic {
    fn drop(&mut self) {
        let Some(before) = self.0 else { return };
        let now: [usize; METADATA_REFUSAL_STAGES] =
            std::array::from_fn(|index| METADATA_REFUSALS[index].load(Ordering::Relaxed));
        if now == before || !reserve_metadata_refusal_log(&METADATA_REFUSAL_LOGS) {
            return;
        }
        let stages = [
            "capture_terminal",
            "capture_screen",
            "publish_terminal",
            "publish_screen",
            "layout_terminal",
            "publish_layout_terminal",
            "layout_observation",
            "sink_interval",
            "surface_terminal",
            "surface_tmux",
            "layout_unavailable",
            "dimensions_unavailable",
            "sink_usage",
            "surface_geometry",
            "publication_geometry",
        ];
        let totals: [(&str, usize); METADATA_REFUSAL_STAGES] =
            std::array::from_fn(|index| (stages[index], now[index]));
        log::debug!(target: "mux::metadata_refusal", "metadata_refusal_totals={totals:?}");
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionAnchorCaptureError {
    Busy,
    SourceChanged,
}

type SelectionPoints = [Option<frankenterm_term::screen::SelectionAnchorCoordinate>; 3];

#[derive(Clone, PartialEq)]
enum ColdSelectionRequest {
    Capture {
        floor: SequenceNo,
        sequence: SequenceNo,
        dimensions: RenderableDimensions,
        points: SelectionPoints,
    },
    Resolve(frankenterm_term::screen::ScreenSelectionAnchorWeak),
}

#[derive(Clone)]
enum ColdSelectionValue {
    Busy,
    RetryAt(Instant),
    Captured(
        Result<
            Option<frankenterm_term::screen::ScreenSelectionAnchor>,
            SelectionAnchorCaptureError,
        >,
    ),
    Resolved(Option<SelectionPoints>),
}

#[derive(Clone)]
struct ColdSelectionReady {
    floor: SequenceNo,
    sequence: SequenceNo,
    dimensions: RenderableDimensions,
    value: ColdSelectionValue,
}

const MAX_COLD_SELECTION_WORK: usize = 16;
const COLD_SELECTION_WORK_LEASE: Duration = Duration::from_secs(5);

// Independent gestures cannot cancel one another's hydration. Finished work
// retains only an opaque token or three coordinates; the shared LineReadPermit
// still bounds all decoded payloads and active workers.
struct ColdSelectionWork {
    request: ColdSelectionRequest,
    cancelled: Arc<AtomicBool>,
    ready: Option<ColdSelectionReady>,
    renewed: Instant,
}

struct ColdSelectionCompletion {
    slot: Arc<Mutex<Vec<ColdSelectionWork>>>,
    cancelled: Arc<AtomicBool>,
}

impl Drop for ColdSelectionCompletion {
    fn drop(&mut self) {
        let mut slot = self.slot.lock();
        slot.retain(|work| !Arc::ptr_eq(&work.cancelled, &self.cancelled) || work.ready.is_some());
    }
}

type LineLayoutObservation = Mutex<
    Option<(
        frankenterm_term::screen::ScreenCoordinateWitness,
        SequenceNo,
    )>,
>;

/// A scrolled viewport retains its logical insertion point across reflow.
/// Resident rows use terminal anchors; closed cold groups use storage identity.
#[derive(Clone)]
pub struct NativeViewport {
    row: StableRowIndex,
    anchor: Option<frankenterm_term::screen::ScreenSelectionAnchor>,
    cold_anchor: Option<frankenterm_term::screen::ColdViewportAnchor>,
}

impl NativeViewport {
    pub fn new(row: StableRowIndex) -> Self {
        Self {
            row,
            anchor: None,
            cold_anchor: None,
        }
    }
}

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
    pub viewport: Option<NativeViewport>,
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
    Option<ColdViewportFollowup>,
);

struct ColdViewportFollowup {
    read: frankenterm_term::screen::ScreenLineRead,
    requested: Range<StableRowIndex>,
    layout: (SequenceNo, RenderableDimensions),
}

#[derive(Clone)]
struct ColdViewportIntent {
    anchor: frankenterm_term::screen::ColdViewportAnchor,
    dimensions: RenderableDimensions,
}

// Return both rejected publications and evictions to the originating worker.
// That worker keeps its permit until this queue is drained and destroyed.
struct ColdViewportRetirement {
    read: Option<Arc<frankenterm_term::screen::ScreenLineRead>>,
    evicted: Vec<ColdViewportEntry>,
    sender: std::sync::mpsc::SyncSender<ColdViewportRetired>,
    followup: Option<ColdViewportFollowup>,
}

impl Drop for ColdViewportRetirement {
    fn drop(&mut self) {
        if let Some(read) = self.read.take() {
            let _ = self.sender.send((
                read,
                std::mem::take(&mut self.evicted),
                self.followup.take(),
            ));
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

#[derive(Clone)]
struct ColdViewportFailure {
    requested: Range<StableRowIndex>,
    witness: frankenterm_term::screen::LineReadFailureWitness,
    retry_at: Instant,
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
    spawn_custody: Option<crate::guardian_checkpoint::GuardianSpawnCaptureProvenanceV1>,
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
    #[error("pending parser action admission refused before mutation: {0:?}")]
    ActionAdmission(PaneActionAdmissionRefusal),
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
        spawn_custody: Option<crate::guardian_checkpoint::GuardianSpawnCaptureProvenanceV1>,
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

fn next_search_grapheme_idx(last_grapheme_idx: usize, width: usize) -> usize {
    last_grapheme_idx.saturating_add(width)
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
    cold_selection: Arc<Mutex<Vec<ColdSelectionWork>>>,
    cold_selection_retry: Arc<AtomicBool>,
    line_layout_observation: Arc<LineLayoutObservation>,
    // Serializes complete producer batches, including deferred persistence,
    // without preventing GUI readers or resize workers from taking terminal.
    output_application: Mutex<()>,
    alert_staging: Arc<Mutex<PaneAlertStaging>>,
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
    action_ring: Arc<ArrayQueue<AdmittedPaneActions>>,
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
    fn capture_selection_anchor_capability(
        &self,
        sequence: SequenceNo,
        dimensions: RenderableDimensions,
        points: [Option<frankenterm_term::screen::SelectionAnchorCoordinate>; 3],
        state: &mut Option<crate::pane::PaneSelectionAnchor>,
    ) -> Result<crate::pane::PaneSelectionAnchorStatus, crate::pane::PaneSelectionAnchorError> {
        use crate::pane::{PaneSelectionAnchorError as Error, PaneSelectionAnchorStatus as Status};
        if state.is_some() {
            return Err(Error::SourceChanged);
        }
        let (floor, current_dimensions) = self
            .get_line_layout()
            .map_err(|_| Error::Busy)?
            .ok_or(Error::Unsupported)?;
        if floor > sequence
            || sequence > self.get_current_seqno()
            || !crate::renderable::same_line_layout_geometry(&dimensions, &current_dimensions)
        {
            return Err(Error::SourceChanged);
        }
        match self.capture_selection_anchor(floor, sequence, current_dimensions, points) {
            Ok(Some(token)) => {
                *state = Some(Box::new(token));
                Ok(Status::Captured)
            }
            Ok(None) => Err(Error::Unsupported),
            Err(SelectionAnchorCaptureError::Busy) => Err(Error::Busy),
            Err(SelectionAnchorCaptureError::SourceChanged) => Err(Error::SourceChanged),
        }
    }

    fn selection_anchor_capability_snapshot(
        &self,
        state: &crate::pane::PaneSelectionAnchor,
    ) -> Result<
        Option<crate::pane::PaneSelectionAnchorSnapshot>,
        crate::pane::PaneSelectionAnchorError,
    > {
        let token = state
            .downcast_ref::<frankenterm_term::screen::ScreenSelectionAnchor>()
            .ok_or(crate::pane::PaneSelectionAnchorError::SourceChanged)?;
        Ok(self.selection_anchor_snapshot(token))
    }

    fn guardian_spawn_custody(
        &self,
    ) -> Option<crate::guardian_checkpoint::GuardianSpawnCaptureProvenanceV1> {
        match &self.ownership {
            LocalPaneOwnership::Guardian(owner) => owner.spawn_custody,
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
            let (first, mut snapshot) = self.cold_viewport_lines(logical_context, None);
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
        let _diagnostic = MetadataRefusalDiagnostic::new();
        Some(
            self.terminal
                .try_lock()
                .ok_or_else(|| {
                    anyhow::Error::new(metadata_busy(MetadataRefusalStage::CaptureTerminal))
                })
                .and_then(|term| {
                    term.screen()
                        .capture_line_read_with_budget(lines, budget)
                        .inspect_err(|error| {
                            if error.is::<frankenterm_term::screen::ColdReadMetadataBusy>() {
                                record_metadata_refusal(MetadataRefusalStage::CaptureScreen);
                            }
                        })
                }),
        )
    }

    fn publish_line_reads(
        &self,
        reads: &[frankenterm_term::screen::ScreenLineRead],
        publish: &mut dyn FnMut(),
    ) -> Result<bool, frankenterm_term::screen::ColdReadMetadataBusy> {
        let _diagnostic = MetadataRefusalDiagnostic::new();
        let mut term = self
            .terminal
            .try_lock()
            .ok_or_else(|| metadata_busy(MetadataRefusalStage::PublishTerminal))?;
        for read in reads {
            if !term
                .screen()
                .try_validate_line_read(read)
                .inspect_err(|_| {
                    record_metadata_refusal(MetadataRefusalStage::PublishScreen);
                })?
            {
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
        let _diagnostic = MetadataRefusalDiagnostic::new();
        let mut term = self
            .terminal
            .try_lock()
            .ok_or_else(|| metadata_busy(MetadataRefusalStage::LayoutTerminal))?;
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
        let _diagnostic = MetadataRefusalDiagnostic::new();
        let mut term = self
            .terminal
            .try_lock()
            .ok_or_else(|| metadata_busy(MetadataRefusalStage::PublishLayoutTerminal))?;
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
            record_metadata_refusal(MetadataRefusalStage::PublicationGeometry);
            return Ok(false);
        }
        for read in reads {
            if !term
                .screen()
                .try_validate_line_read(read)
                .inspect_err(|_| {
                    record_metadata_refusal(MetadataRefusalStage::PublishScreen);
                })?
            {
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
        let _diagnostic = MetadataRefusalDiagnostic::new();
        let mut term = match self.terminal.try_lock() {
            Some(term) => term,
            None => return Some(Err(metadata_busy(MetadataRefusalStage::SurfaceTerminal))),
        };
        #[cfg(feature = "disruptor-pane-io")]
        self.drain_action_ring_locked(&mut term);

        match self.tmux_domain.try_lock() {
            Some(guard) if guard.is_some() => return None,
            Some(_) => {}
            None => return Some(Err(metadata_busy(MetadataRefusalStage::SurfaceTmux))),
        }

        let line_layout_floor =
            match Self::refresh_line_layout_floor(&self.line_layout_observation, &mut term) {
                Ok(Some(floor)) => floor,
                // Let the existing source-fence path retire an exhausted
                // sequence domain instead of turning it into retryable busy.
                Ok(None) if term.current_seqno() == SequenceNo::MAX => return None,
                Ok(None) => {
                    return Some(Err(metadata_busy(MetadataRefusalStage::LayoutUnavailable)))
                }
                Err(busy) => return Some(Err(busy)),
            };

        let dimensions = match terminal_try_get_dimensions(&mut term) {
            Some(dims) => dims,
            None => {
                return Some(Err(metadata_busy(
                    MetadataRefusalStage::DimensionsUnavailable,
                )))
            }
        };

        let mouse_grabbed = term.is_mouse_grabbed();
        let alt_screen_active = term.is_alt_screen_active();

        let tiered_scrollback_status = match term.screen().try_tiered_scrollback_status() {
            Ok(status) => status.map(Into::into),
            Err(busy) => {
                record_metadata_refusal(MetadataRefusalStage::SinkUsage);
                return Some(Err(busy));
            }
        };

        let cursor_position = terminal_get_cursor_position(&mut term);

        let source_sequence = term.current_seqno();

        let damage_baseline = crate::pane::changed_since_query_baseline(baseline, source_sequence);

        let viewport_range = match StableRowIndex::try_from(dimensions.viewport_rows)
            .ok()
            .and_then(|len| dimensions.physical_top.checked_add(len))
        {
            Some(end) => dimensions.physical_top..end,
            None => return Some(Err(metadata_busy(MetadataRefusalStage::SurfaceGeometry))),
        };

        let dirty_lines = terminal_get_dirty_lines(&mut term, viewport_range, damage_baseline);

        let screen = term.screen();

        let phys_start = match screen.stable_row_to_phys(dimensions.physical_top) {
            Some(phys) => phys,
            None => return Some(Err(metadata_busy(MetadataRefusalStage::SurfaceGeometry))),
        };
        let phys_end = match phys_start.checked_add(dimensions.viewport_rows) {
            Some(end) => end,
            None => return Some(Err(metadata_busy(MetadataRefusalStage::SurfaceGeometry))),
        };
        let lines = screen.lines_in_phys_range(phys_start..phys_end);
        if lines.len() != dimensions.viewport_rows {
            return Some(Err(metadata_busy(MetadataRefusalStage::SurfaceGeometry)));
        }
        let viewport_lines = (dimensions.physical_top, lines);

        let cursor_phys = match screen.stable_row_to_phys(cursor_position.y) {
            Some(phys) => phys,
            None => return Some(Err(metadata_busy(MetadataRefusalStage::SurfaceGeometry))),
        };
        let cursor_end = match cursor_phys.checked_add(1) {
            Some(end) => end,
            None => return Some(Err(metadata_busy(MetadataRefusalStage::SurfaceGeometry))),
        };
        let cursor_row_lines = screen.lines_in_phys_range(cursor_phys..cursor_end);
        if cursor_row_lines.len() != 1 {
            return Some(Err(metadata_busy(MetadataRefusalStage::SurfaceGeometry)));
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

    fn perform_actions(
        &self,
        mut actions: Vec<termwiz::escape::Action>,
    ) -> Result<(), PaneActionAdmissionError> {
        if actions.is_empty() {
            return Ok(());
        }
        // Authority is acquired outside terminal/tab locks and declared before
        // their guards, so refusal drops those guards before lifecycle custody.
        let mut output = match self.prepare_alert_output() {
            Ok(output) => output,
            Err(reason) => return Err(PaneActionAdmissionError { actions, reason }),
        };
        let _output_application = self.output_application.lock();
        if PaneAlertPreflight::needs_terminal_state(&actions) {
            // A terminator may publish a title begun in another batch. Drain
            // prior admitted work, then observe and admit against exact state.
            let mut terminal = self.locked_terminal();
            let pending_title = terminal.pending_tmux_title_bytes();
            match self.admit_alert_actions(&mut actions, pending_title, &mut output) {
                Ok(batch) => batch.apply(&mut terminal),
                Err(reason) => return Err(PaneActionAdmissionError { actions, reason }),
            }
        } else {
            let batch = match self.admit_alert_actions(&mut actions, 0, &mut output) {
                Ok(batch) => batch,
                Err(reason) => return Err(PaneActionAdmissionError { actions, reason }),
            };
            #[cfg(not(feature = "disruptor-pane-io"))]
            batch.apply(&mut self.terminal.lock());
            #[cfg(feature = "disruptor-pane-io")]
            self.perform_actions_disruptor(batch);
        }
        // With the disruptor enabled this also drains the admitted ring before
        // backpressure, so queued rows cannot be stranded until later input.
        let sink = self.scrollback_flush_sink.lock().clone();
        if let Some(sink) = sink {
            drop(self.locked_terminal());
            self.drain_scrollback_outside_terminal(sink);
        }
        Ok(())
    }

    fn capture_live_parser_checkpoint(
        &self,
        _authority: LiveParserCaptureAuthority,
        pending_actions: &mut Vec<Action>,
        ground: termwiz::escape::parser::RecoveryGroundBoundary<'_>,
        limits: TerminalCheckpointLimits,
    ) -> Result<RecoveryTerminalCheckpointV3, LiveParserPaneCaptureError> {
        let mut output = if pending_actions.is_empty() {
            None
        } else {
            self.prepare_alert_output()
                .map_err(LiveParserPaneCaptureError::ActionAdmission)?
        };
        // `locked_terminal` drains the optional disruptor ring before it returns.
        // Apply this parser's still-local actions and capture staged hot state
        // under the terminal lock, then release the lock immediately so cold-history
        // materialization and serialization do not block the terminal mutex.
        let staged = {
            let _output_application = self.output_application.lock();
            let mut terminal = self.locked_terminal();
            let pending_title = terminal.pending_tmux_title_bytes();
            self.admit_alert_actions(pending_actions, pending_title, &mut output)
                .map_err(LiveParserPaneCaptureError::ActionAdmission)?
                .apply(&mut terminal);
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
        const CHUNK: StableRowIndex = 1000;
        const CONTEXT: StableRowIndex = 1024;
        const MAX_RESULTS: usize = 100_000;
        struct CancelOnDrop(Arc<AtomicBool>);
        impl Drop for CancelOnDrop {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        'restart: for _ in 0..3 {
            let cancelled = Arc::new(AtomicBool::new(false));
            let _cancel = CancelOnDrop(Arc::clone(&cancelled));
            let (sequence, history, resident_start) = {
                let term = self.locked_terminal();
                let (first, count) = term.screen().scrollback_geometry();
                let end = first
                    .checked_add(StableRowIndex::try_from(count)?)
                    .ok_or_else(|| anyhow::anyhow!("search history range overflow"))?;
                anyhow::ensure!(
                    term.current_seqno() != SequenceNo::MAX,
                    "search source saturated"
                );
                (
                    term.current_seqno(),
                    first..end,
                    term.screen().phys_to_stable_row_index(0),
                )
            };
            let range = range.start.max(history.start)..range.end.min(history.end);
            let limit = limit.map_or(MAX_RESULTS, |limit| (limit as usize).min(MAX_RESULTS));
            let mut results = Vec::new();
            let mut unique = SearchMatchSet::default();
            let mut literal = StreamingLiteralSearch::new(&pattern, range.clone(), history.start)?;
            let mut source_witness = None;
            let mut first = range.start;
            while results.len() < limit {
                let end = first.saturating_add(CHUNK).min(range.end);
                let captured = if let Some(literal) = &literal {
                    match literal.capture_range(&history, CHUNK) {
                        Some(range) => range,
                        None => break,
                    }
                } else {
                    if first >= range.end {
                        break;
                    }
                    first.saturating_sub(CONTEXT).max(history.start)
                        ..end.saturating_add(CONTEXT).min(history.end)
                };
                let captured_start = captured.start;
                let captured_end = captured.end;
                let permit = crate::pane::LineReadPermit::try_acquire()
                    .ok_or_else(|| anyhow::anyhow!("search read admission busy"))?;
                let mut completion = promise::Promise::new();
                let future = completion.get_future().expect("new search promise");
                let worker_cancel = Arc::clone(&cancelled);
                let matcher_cancel = Arc::clone(&cancelled);
                let terminal = Arc::clone(&self.terminal);
                let requested = first..end;
                let worker_pattern = pattern.clone();
                let worker_history = history.clone();
                let remaining = limit - results.len();
                let worker = permit.start(
                    move || worker_cancel.load(Ordering::Acquire),
                    move |reads, _permit| {
                        let outcome = (|| {
                            let reads = reads?;
                            let read = reads
                                .first()
                                .ok_or_else(|| anyhow::anyhow!("search read missing"))?;
                            {
                                let mut term = terminal
                                    .try_lock()
                                    .ok_or_else(|| anyhow::anyhow!("search publication busy"))?;
                                anyhow::ensure!(
                                    term.current_seqno() == sequence,
                                    "search source changed"
                                );
                                anyhow::ensure!(
                                    term.screen().try_validate_line_read(read)?,
                                    "search source expired"
                                );
                                // An oldest-row bootstrap may rebase its first
                                // visual row. Publish proven geometry before
                                // interpreting any requested numeric position.
                                if term.screen().line_read_changes_layout(read) {
                                    term.increment_seqno();
                                    let seqno = term.current_seqno();
                                    term.screen_mut().install_line_read_layout(read, seqno);
                                    return Err(SearchLayoutRefreshed.into());
                                }
                            }
                            anyhow::ensure!(
                                read.first_row() == captured_start,
                                "search layout changed"
                            );
                            let lines: Vec<_> = read.lines().collect();
                            let mut unique = unique;
                            let mut literal = literal;
                            let matches = if let Some(literal) = &mut literal {
                                literal.consume(
                                    captured_start..captured_end,
                                    &lines,
                                    remaining,
                                    &mut unique,
                                    &matcher_cancel,
                                )?
                            } else {
                                search_owned_lines(
                                    worker_pattern,
                                    requested,
                                    remaining,
                                    (read.first_row(), &lines),
                                    &worker_history,
                                    &mut unique,
                                    &matcher_cancel,
                                )?
                            };
                            let term = terminal
                                .try_lock()
                                .ok_or_else(|| anyhow::anyhow!("search publication busy"))?;
                            anyhow::ensure!(
                                term.current_seqno() == sequence,
                                "search source changed"
                            );
                            anyhow::ensure!(
                                term.screen().try_validate_line_read(read)?,
                                "search source expired"
                            );
                            anyhow::ensure!(
                                !matcher_cancel.load(Ordering::Acquire),
                                "search cancelled"
                            );
                            Ok((matches, unique, literal))
                        })();
                        completion.result(outcome);
                    },
                )?;
                let plan = {
                    let term = self
                        .terminal
                        .try_lock()
                        .ok_or_else(|| anyhow::anyhow!("search capture busy"))?;
                    anyhow::ensure!(term.current_seqno() == sequence, "search source changed");
                    term.screen().capture_line_read(captured.clone())?
                };
                if source_witness.is_none() && captured.start < resident_start {
                    source_witness = Some(plan.failure_witness());
                }
                let plan = if matches!(pattern, Pattern::Regex(_)) {
                    plan
                } else {
                    plan.with_requested_physical_rows_only()
                };
                worker.submit(vec![plan]);
                let (matches, next_unique, next_literal) = match future.await {
                    Ok(value) => value,
                    Err(error) if error.is::<SearchLayoutRefreshed>() => continue 'restart,
                    Err(error) => return Err(error),
                };
                unique = next_unique;
                literal = next_literal;
                results.extend(matches);
                first = end;
            }
            let term = self
                .terminal
                .try_lock()
                .ok_or_else(|| anyhow::anyhow!("search completion busy"))?;
            anyhow::ensure!(term.current_seqno() == sequence, "search source changed");
            if let Some(witness) = source_witness {
                anyhow::ensure!(
                    witness.matches(term.screen()),
                    "search retained source changed"
                );
            }
            return Ok(results);
        }
        anyhow::bail!("search layout did not settle")
    }
}

#[derive(Debug, thiserror::Error)]
#[error("search layout refreshed")]
struct SearchLayoutRefreshed;

#[derive(Default)]
struct SearchMatchSet {
    values: HashMap<String, usize>,
    retained_bytes: usize,
}

impl SearchMatchSet {
    fn id_for(&mut self, text: &str) -> anyhow::Result<usize> {
        if let Some(id) = self.values.get(text) {
            return Ok(*id);
        }
        let bytes = self
            .retained_bytes
            .checked_add(text.len())
            .filter(|bytes| *bytes <= frankenterm_term::screen::ScreenLineRead::MAX_PAYLOAD_BYTES)
            .ok_or_else(|| anyhow::anyhow!("search unique match text exceeds memory budget"))?;
        let id = self.values.len();
        self.values.insert(text.to_owned(), id);
        self.retained_bytes = bytes;
        Ok(id)
    }
}

#[derive(Clone, Copy)]
struct LiteralByteCoordinate {
    row: StableRowIndex,
    column: usize,
    end_row: StableRowIndex,
    end_column: usize,
}

enum LiteralSearchPhase {
    FindHead(StableRowIndex),
    Forward(StableRowIndex),
    Done,
}

/// KMP state survives payload retirement. Only query-sized byte coordinates
/// are retained; logical paragraphs are never concatenated for literal search.
struct StreamingLiteralSearch {
    needle: String,
    failure: Vec<usize>,
    matched: usize,
    coordinates: std::collections::VecDeque<LiteralByteCoordinate>,
    folded: bool,
    requested: Range<StableRowIndex>,
    history_start: StableRowIndex,
    phase: LiteralSearchPhase,
}

impl StreamingLiteralSearch {
    fn fold(text: &str) -> String {
        // Sigma's final form depends on following text. Normalize both forms
        // in the comparison key so a capture boundary cannot change folding.
        text.chars()
            .flat_map(char::to_lowercase)
            .map(|c| if c == '\u{3c2}' { '\u{3c3}' } else { c })
            .collect()
    }

    fn new(
        pattern: &Pattern,
        requested: Range<StableRowIndex>,
        history_start: StableRowIndex,
    ) -> anyhow::Result<Option<Self>> {
        let (text, folded) = match pattern {
            Pattern::CaseSensitiveString(text) => (text, false),
            Pattern::CaseInSensitiveString(text) => (text, true),
            Pattern::Regex(_) => return Ok(None),
        };
        let bytes_per_query_byte =
            2 * std::mem::size_of::<LiteralByteCoordinate>() + std::mem::size_of::<usize>() + 4;
        let maximum_query_bytes =
            frankenterm_term::screen::ScreenLineRead::MAX_PAYLOAD_BYTES / bytes_per_query_byte;
        anyhow::ensure!(
            text.len() <= maximum_query_bytes / if folded { 3 } else { 1 },
            "search literal exceeds streaming memory budget"
        );
        let needle = if folded {
            Self::fold(text)
        } else {
            text.clone()
        };
        anyhow::ensure!(
            needle.len() <= maximum_query_bytes,
            "search folded literal exceeds streaming memory budget"
        );
        let mut failure = Vec::new();
        failure.try_reserve_exact(needle.len())?;
        failure.resize(needle.len(), 0);
        let bytes = needle.as_bytes();
        let mut matched = 0;
        for index in 1..bytes.len() {
            while matched > 0 && bytes[matched] != bytes[index] {
                matched = failure[matched - 1];
            }
            if bytes[matched] == bytes[index] {
                matched += 1;
            }
            failure[index] = matched;
        }
        let mut coordinates = std::collections::VecDeque::new();
        coordinates.try_reserve_exact(needle.len())?;
        let phase = if needle.is_empty() || requested.is_empty() {
            LiteralSearchPhase::Done
        } else if requested.start == history_start {
            LiteralSearchPhase::Forward(history_start)
        } else {
            LiteralSearchPhase::FindHead(requested.start)
        };
        Ok(Some(Self {
            needle,
            failure,
            matched: 0,
            coordinates,
            folded,
            requested,
            history_start,
            phase,
        }))
    }

    fn capture_range(
        &self,
        history: &Range<StableRowIndex>,
        chunk: StableRowIndex,
    ) -> Option<Range<StableRowIndex>> {
        match self.phase {
            LiteralSearchPhase::FindHead(end) => {
                Some(end.saturating_sub(chunk).max(history.start)..end)
            }
            LiteralSearchPhase::Forward(first) if first < history.end => {
                Some(first..first.saturating_add(chunk).min(history.end))
            }
            _ => None,
        }
    }

    fn consume(
        &mut self,
        captured: Range<StableRowIndex>,
        lines: &[&Line],
        limit: usize,
        unique: &mut SearchMatchSet,
        cancelled: &AtomicBool,
    ) -> anyhow::Result<Vec<SearchResult>> {
        anyhow::ensure!(
            usize::try_from(
                captured
                    .end
                    .checked_sub(captured.start)
                    .ok_or_else(|| anyhow::anyhow!("search literal row range overflow"))?
            )? == lines.len(),
            "search literal capture has a row gap"
        );
        if let LiteralSearchPhase::FindHead(expected_end) = self.phase {
            anyhow::ensure!(
                captured.end == expected_end,
                "search literal prefix discontinuity"
            );
            for (offset, line) in lines.iter().enumerate().rev() {
                anyhow::ensure!(!cancelled.load(Ordering::Acquire), "search cancelled");
                if !line.last_cell_was_wrapped() {
                    self.phase =
                        LiteralSearchPhase::Forward(captured.start + offset as StableRowIndex + 1);
                    return Ok(Vec::new());
                }
            }
            self.phase = if captured.start == self.history_start {
                LiteralSearchPhase::Forward(captured.start)
            } else {
                LiteralSearchPhase::FindHead(captured.start)
            };
            return Ok(Vec::new());
        }
        anyhow::ensure!(
            matches!(self.phase, LiteralSearchPhase::Forward(row) if row == captured.start),
            "search literal payload discontinuity"
        );
        let mut results = Vec::new();
        for (offset, line) in lines.iter().enumerate() {
            anyhow::ensure!(!cancelled.load(Ordering::Acquire), "search cancelled");
            let row = captured.start + offset as StableRowIndex;
            if row >= self.requested.end && self.matched == 0 {
                self.phase = LiteralSearchPhase::Done;
                return Ok(results);
            }
            let mut cells = line.visible_cells().peekable();
            while let Some(cell) = cells.next() {
                anyhow::ensure!(!cancelled.load(Ordering::Acquire), "search cancelled");
                let wrap_end = cells.peek().is_none() && line.last_cell_was_wrapped();
                let coordinate = LiteralByteCoordinate {
                    row,
                    column: cell.cell_index(),
                    end_row: if wrap_end { row + 1 } else { row },
                    end_column: if wrap_end {
                        0
                    } else {
                        cell.cell_index() + cell.width()
                    },
                };
                for character in cell.str().chars() {
                    // Lowercase expansion is at most three Unicode scalars.
                    // Encode one scalar's comparison bytes at a time rather
                    // than allocating a folded copy of a large grapheme.
                    let mut encoded = [0u8; 12];
                    let mut count = 0;
                    if self.folded {
                        for lowered in character.to_lowercase() {
                            let lowered = if lowered == '\u{3c2}' {
                                '\u{3c3}'
                            } else {
                                lowered
                            };
                            count += lowered.encode_utf8(&mut encoded[count..]).len();
                        }
                    } else {
                        count = character.encode_utf8(&mut encoded).len();
                    }
                    for byte in encoded[..count].iter().copied() {
                        anyhow::ensure!(!cancelled.load(Ordering::Acquire), "search cancelled");
                        if self.coordinates.len() == self.needle.len() {
                            self.coordinates.pop_front();
                        }
                        self.coordinates.push_back(coordinate);
                        while self.matched > 0 && self.needle.as_bytes()[self.matched] != byte {
                            self.matched = self.failure[self.matched - 1];
                        }
                        if self.needle.as_bytes()[self.matched] == byte {
                            self.matched += 1;
                        }
                        if self.matched == self.needle.len() {
                            let start = self
                                .coordinates
                                .front()
                                .expect("complete literal has coordinates");
                            if self.requested.contains(&start.row) {
                                results.push(SearchResult {
                                    start_x: start.column,
                                    start_y: start.row,
                                    end_x: coordinate.end_column,
                                    end_y: coordinate.end_row,
                                    match_id: unique.id_for(&self.needle)?,
                                });
                            }
                            // Match the non-overlapping literal semantics of
                            // str::match_indices, including across capture windows.
                            self.matched = 0;
                            if results.len() == limit {
                                self.phase = LiteralSearchPhase::Done;
                                return Ok(results);
                            }
                        }
                        if row >= self.requested.end {
                            let eligible_prefix = self.matched > 0
                                && self.coordinates[self.coordinates.len() - self.matched].row
                                    < self.requested.end;
                            if !eligible_prefix {
                                self.phase = LiteralSearchPhase::Done;
                                return Ok(results);
                            }
                        }
                    }
                }
            }
            if !line.last_cell_was_wrapped() {
                self.matched = 0;
                self.coordinates.clear();
            }
        }
        self.phase = LiteralSearchPhase::Forward(captured.end);
        Ok(results)
    }
}

fn search_owned_lines(
    pattern: Pattern,
    range: Range<StableRowIndex>,
    limit: usize,
    (first_row, physical): (StableRowIndex, &[&Line]),
    history: &Range<StableRowIndex>,
    uniq_matches: &mut SearchMatchSet,
    cancelled: &AtomicBool,
) -> anyhow::Result<Vec<SearchResult>> {
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
    let folded = matches!(pattern, CompiledPattern::CaseInSensitiveString(_));
    let mut match_error: Option<anyhow::Error> = None;
    search_logical_lines(
        first_row,
        physical,
        &range,
        history,
        cancelled,
        |sr, lines| {
            if results.len() >= limit || cancelled.load(Ordering::Acquire) {
                // We've reach the limit, stop iteration.
                return false;
            }

            if lines.is_empty() {
                // Nothing to do on this iteration, carry on with the next.
                return true;
            }
            // Bound concatenation, case-fold expansion and the lazily allocated
            // coordinate map together, before allocating matcher scratch space.
            let mut haystack_bytes = 0usize;
            let mut scratch_bytes = 0usize;
            for line in lines {
                if cancelled.load(Ordering::Acquire) {
                    return false;
                }
                let bytes = line.as_str().len();
                let bounded = (|| {
                    haystack_bytes = haystack_bytes.checked_add(bytes)?;
                    scratch_bytes = scratch_bytes
                        .checked_add(bytes.checked_mul(if folded { 4 } else { 2 })?)?
                        .checked_add(line.len().checked_mul(2 * std::mem::size_of::<Coord>())?)?;
                    (scratch_bytes <= frankenterm_term::screen::ScreenLineRead::MAX_PAYLOAD_BYTES)
                        .then_some(())
                })();
                if bounded.is_none() {
                    match_error = Some(anyhow::anyhow!(
                        "search logical group exceeds matcher memory budget"
                    ));
                    return false;
                }
            }
            let haystack = if lines.len() == 1 {
                lines[0].as_str()
            } else {
                let mut s = String::with_capacity(haystack_bytes);
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
                        if results.len() >= limit || cancelled.load(Ordering::Acquire) {
                            break;
                        }
                        if let Err(error) = found_match(
                            s,
                            idx,
                            lines,
                            stable_idx,
                            uniq_matches,
                            &mut coords,
                            &mut results,
                            &range,
                            folded,
                        ) {
                            match_error = Some(error);
                            return false;
                        }
                    }
                }
                CompiledPattern::Regex(re) => {
                    // Allow for the regex to contain captures
                    for capture_res in re.captures_iter(&*haystack) {
                        if results.len() >= limit || cancelled.load(Ordering::Acquire) {
                            break;
                        }
                        let c = match capture_res {
                            Ok(c) => c,
                            Err(error) => {
                                match_error = Some(error.into());
                                return false;
                            }
                        };
                        {
                            // Look for the captures in reverse order, as index==0 is
                            // the whole matched string.  We can't just call
                            // `c.iter().rev()` as the capture iterator isn't double-ended.
                            for idx in (0..c.len()).rev() {
                                if let Some(m) = c.get(idx) {
                                    if let Err(error) = found_match(
                                        m.as_str(),
                                        m.start(),
                                        lines,
                                        stable_idx,
                                        uniq_matches,
                                        &mut coords,
                                        &mut results,
                                        &range,
                                        folded,
                                    ) {
                                        match_error = Some(error);
                                        return false;
                                    }
                                    break;
                                }
                            }
                        }
                    }
                }
            }

            // Keep iterating
            results.len() < limit
        },
    )?;

    #[derive(Copy, Clone, Debug)]
    struct Coord {
        byte_idx: usize,
        grapheme_idx: usize,
        width: usize,
        stable_row: StableRowIndex,
    }

    fn found_match(
        text: &str,
        byte_idx: usize,
        lines: &[&Line],
        stable_idx: StableRowIndex,
        uniq_matches: &mut SearchMatchSet,
        coords: &mut Option<Vec<Coord>>,
        results: &mut Vec<SearchResult>,
        range: &Range<StableRowIndex>,
        folded: bool,
    ) -> anyhow::Result<()> {
        // Zero-width regex alternatives are not selectable text. Do not let
        // them exhaust the result budget before a later nonempty match.
        if text.is_empty() {
            return Ok(());
        }
        if coords.is_none() {
            coords.replace(make_coords(lines, stable_idx, folded));
        }
        let Some(coords) = coords.as_ref() else {
            return Ok(());
        };
        if coords.is_empty() {
            return Ok(());
        }

        let (start_x, start_y) = haystack_idx_to_coord(byte_idx, coords, false);
        if !range.contains(&start_y) {
            return Ok(());
        }
        let match_id = uniq_matches.id_for(text)?;
        let (end_x, end_y) = haystack_idx_to_coord(byte_idx + text.len(), coords, true);
        results.push(SearchResult {
            start_x,
            start_y,
            end_x,
            end_y,
            match_id,
        });
        Ok(())
    }

    fn make_coords(lines: &[&Line], stable_row: StableRowIndex, folded: bool) -> Vec<Coord> {
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
                    width: cell.width(),
                    stable_row,
                });
                byte_idx += if folded {
                    cell.str().to_lowercase().len()
                } else {
                    cell.str().len()
                };
            }
        }

        coords
    }

    fn haystack_idx_to_coord(
        idx: usize,
        coords: &[Coord],
        is_end: bool,
    ) -> (usize, StableRowIndex) {
        let c = match coords.binary_search_by(|ele| ele.byte_idx.cmp(&idx)) {
            Ok(index) => index,
            // A Unicode match can start inside one displayed grapheme (or
            // inside its case-fold expansion). Select that containing cell;
            // the exclusive end still rounds up to include the entire cell.
            Err(index) if !is_end => index.saturating_sub(1),
            Err(index) => index,
        };
        let coord = coords.get(c).map(|c| *c).unwrap_or_else(|| {
            let Some(last) = coords.last() else {
                return Coord {
                    byte_idx: 0,
                    grapheme_idx: 0,
                    width: 0,
                    stable_row: 0,
                };
            };
            Coord {
                grapheme_idx: next_search_grapheme_idx(last.grapheme_idx, last.width),
                ..*last
            }
        });
        (coord.grapheme_idx, coord.stable_row)
    }

    if let Some(error) = match_error {
        return Err(error);
    }
    anyhow::ensure!(!cancelled.load(Ordering::Acquire), "search cancelled");
    results.retain(|result| range.contains(&result.start_y));
    results.truncate(limit);
    Ok(results)
}

fn search_logical_lines(
    first: StableRowIndex,
    physical: &[&Line],
    requested: &Range<StableRowIndex>,
    history: &Range<StableRowIndex>,
    cancelled: &AtomicBool,
    mut visit: impl FnMut(Range<StableRowIndex>, &[&Line]) -> bool,
) -> anyhow::Result<()> {
    let mut start = usize::try_from(requested.start.saturating_sub(first))
        .unwrap_or(usize::MAX)
        .min(physical.len());
    while start > 0 && physical[start - 1].last_cell_was_wrapped() {
        anyhow::ensure!(!cancelled.load(Ordering::Acquire), "search cancelled");
        start -= 1;
    }
    // A match can cross any physical row boundary. Splitting a logical line
    // at an arbitrary cell count loses literals and changes regex anchors.
    // Hydration already bounds rows and payload; if its context is incomplete,
    // refuse rather than report an authoritative empty or partial result.
    anyhow::ensure!(
        start != 0 || first == history.start || physical.is_empty(),
        "search logical group exceeds captured prefix context"
    );
    while start < physical.len() && first.saturating_add(start as StableRowIndex) < requested.end {
        let mut end = start;
        while end < physical.len() {
            anyhow::ensure!(!cancelled.load(Ordering::Acquire), "search cancelled");
            end += 1;
            if !physical[end - 1].last_cell_was_wrapped() {
                break;
            }
        }
        anyhow::ensure!(
            end < physical.len()
                || !physical[end - 1].last_cell_was_wrapped()
                || first.saturating_add(end as StableRowIndex) == history.end,
            "search logical group exceeds captured suffix context"
        );
        let rows = first.saturating_add(start as StableRowIndex)
            ..first.saturating_add(end as StableRowIndex);
        if !visit(rows, &physical[start..end]) {
            break;
        }
        start = end;
    }
    Ok(())
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
                apply_generated_actions(&registration, vec![action]);
            }
        ),
        "terminal control could not be queued; background output capacity is unavailable"
    );
    Ok(())
}

pub(crate) fn emit_output_for_pane(registration: PaneRegistrationHandle, message: &str) {
    spawn_generated_output(&GENERATED_OUTPUT_WORKERS, message, move |actions| {
        apply_generated_actions(&registration, actions);
    });
}

fn apply_generated_actions(registration: &PaneRegistrationHandle, mut actions: Vec<Action>) {
    loop {
        match registration.try_with_current_output(|pane| pane.perform_actions(actions)) {
            Some(Ok(())) => return,
            Some(Err(error)) if error.reason == PaneActionAdmissionRefusal::Capacity => {
                actions = error.actions;
                std::thread::sleep(Duration::from_millis(1));
            }
            Some(Err(error)) => {
                metrics::counter!("mux.generated_output.admission_cancelled").increment(1);
                log::error!("generated output cancelled after admission refusal: {error}");
                return;
            }
            None => {
                metrics::counter!("mux.generated_output.registration_cancelled").increment(1);
                return;
            }
        }
    }
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

/// Storage needed before applying an action batch that can emit alerts.
///
/// This is a pre-mutation bound, not an estimate computed after the terminal
/// has already accepted an event. Keep the OSC match exhaustive so new string
/// commands cannot silently bypass the historical-payload budget.
struct PaneAlertPreflight {
    count: usize,
    text_bytes: usize,
    historical: Vec<crate::HistoricalAlertDemand>,
}

impl PaneAlertPreflight {
    fn needs_terminal_state(actions: &[Action]) -> bool {
        actions.iter().any(|action| {
            matches!(
                action,
                Action::Esc(termwiz::escape::Esc::Code(
                    termwiz::escape::EscCode::StringTerminator
                ))
            )
        })
    }

    fn for_actions(actions: &[Action], pending_tmux_title_bytes: usize) -> Option<Self> {
        use termwiz::escape::osc::{ITermProprietary, OperatingSystemCommand};
        use termwiz::escape::{ControlCode, Esc, EscCode};

        // Only ST can publish text retained by earlier batches. Those batches
        // are admitted while holding the terminal lock after draining the ring.
        // Include all printable input, even outside a title, to conservatively
        // cover every same-batch title without assuming String growth policy.
        let mut tmux_title_bytes = pending_tmux_title_bytes;
        if Self::needs_terminal_state(actions) {
            for action in actions {
                let bytes = match action {
                    Action::Print(c) => c.len_utf8(),
                    Action::PrintString(s) => s.len(),
                    _ => 0,
                };
                tmux_title_bytes = tmux_title_bytes.checked_add(bytes)?;
            }
        }
        let mut count = usize::from(!actions.is_empty());
        let mut text_bytes = 0usize;
        let mut historical = Vec::new();
        for action in actions {
            let demand = match action {
                Action::Control(ControlCode::Bell) => Some(crate::HistoricalAlertDemand::Bell),
                Action::OperatingSystemCommand(command) => match command.as_ref() {
                    OperatingSystemCommand::ITermProprietary(ITermProprietary::SetUserVar {
                        name,
                        value,
                    }) => Some(crate::HistoricalAlertDemand::UserVar {
                        text_bytes: name.capacity().checked_add(value.capacity())?,
                    }),
                    _ => None,
                },
                _ => None,
            };
            if let Some(demand) = demand {
                if historical.len() == 256 || historical.try_reserve(1).is_err() {
                    return None;
                }
                historical.push(demand);
            }
            let action_count = match action {
                Action::OperatingSystemCommand(command) => match command.as_ref() {
                    OperatingSystemCommand::SetIconNameAndWindowTitle(_) => 2,
                    // At most one notification per OSC, including palette lists.
                    _ => 1,
                },
                Action::Esc(Esc::Code(EscCode::StringTerminator)) => 2,
                Action::Esc(Esc::Code(EscCode::FullReset))
                | Action::Control(ControlCode::Bell)
                | Action::KittyImage(_) => 1,
                Action::Print(_)
                | Action::PrintString(_)
                | Action::Control(_)
                | Action::DeviceControl(_)
                | Action::CSI(_)
                | Action::Esc(_)
                | Action::Sixel(_)
                | Action::XtGetTcap(_) => 0,
            };
            count = count.checked_add(action_count)?;
            let additional = match action {
                Action::OperatingSystemCommand(command) => match command.as_ref() {
                    OperatingSystemCommand::SetIconNameAndWindowTitle(title) => {
                        title.capacity().checked_mul(2)?
                    }
                    OperatingSystemCommand::SetWindowTitle(title)
                    | OperatingSystemCommand::SetWindowTitleSun(title)
                    | OperatingSystemCommand::SetIconName(title)
                    | OperatingSystemCommand::SetIconNameSun(title)
                    | OperatingSystemCommand::SystemNotification(title)
                    | OperatingSystemCommand::SetMouseShape(title) => title.capacity(),
                    OperatingSystemCommand::ITermProprietary(command) => match command {
                        ITermProprietary::SetUserVar { name, value } => {
                            name.capacity().checked_add(value.capacity())?
                        }
                        ITermProprietary::SetProfile(name) => name.capacity(),
                        ITermProprietary::SetMark
                        | ITermProprietary::StealFocus
                        | ITermProprietary::ClearScrollback
                        | ITermProprietary::CurrentDir(_)
                        | ITermProprietary::CopyToClipboard(_)
                        | ITermProprietary::EndCopy
                        | ITermProprietary::HighlightCursorLine(_)
                        | ITermProprietary::RequestCellSize
                        | ITermProprietary::ReportCellSize { .. }
                        | ITermProprietary::Copy(_)
                        | ITermProprietary::ReportVariable(_)
                        | ITermProprietary::SetBadgeFormat(_)
                        | ITermProprietary::File(_)
                        | ITermProprietary::UnicodeVersion(_) => 0,
                    },
                    OperatingSystemCommand::RxvtExtension(params) => params
                        .get(1)
                        .map_or(0, String::capacity)
                        .checked_add(params.get(2).map_or(0, String::capacity))?,
                    OperatingSystemCommand::SetHyperlink(_)
                    | OperatingSystemCommand::ClearSelection(_)
                    | OperatingSystemCommand::QuerySelection(_)
                    | OperatingSystemCommand::SetSelection(_, _)
                    | OperatingSystemCommand::FinalTermSemanticPrompt(_)
                    | OperatingSystemCommand::ChangeColorNumber(_)
                    | OperatingSystemCommand::ChangeDynamicColors(_, _)
                    | OperatingSystemCommand::ResetDynamicColor(_)
                    | OperatingSystemCommand::CurrentWorkingDirectory(_)
                    | OperatingSystemCommand::ResetColors(_)
                    | OperatingSystemCommand::ConEmuProgress(_)
                    | OperatingSystemCommand::Unspecified(_) => 0,
                },
                // Staged strings are normalized to capacity == length. The
                // sanitizer retains at most 256 Unicode scalar values.
                Action::KittyImage(_) => 256 * 4,
                Action::Esc(Esc::Code(EscCode::StringTerminator)) => {
                    tmux_title_bytes.checked_mul(2)?
                }
                Action::Print(_)
                | Action::PrintString(_)
                | Action::Control(_)
                | Action::DeviceControl(_)
                | Action::CSI(_)
                | Action::Esc(_)
                | Action::Sixel(_)
                | Action::XtGetTcap(_) => 0,
            };
            text_bytes = text_bytes.checked_add(additional)?;
        }
        Some(Self {
            count,
            text_bytes,
            historical,
        })
    }

    fn retained_bytes(&self) -> Option<usize> {
        self.count
            .checked_mul(std::mem::size_of::<Alert>())?
            .checked_add(self.text_bytes)?
            .checked_add(
                self.historical
                    .capacity()
                    .checked_mul(std::mem::size_of::<crate::HistoricalAlertDemand>())?,
            )?
            .checked_add(LOCAL_PANE_MAIN_THREAD_ESTIMATED_BYTES)
    }
}

struct LocalPaneNotifHandler {
    staging: Arc<Mutex<PaneAlertStaging>>,
}

#[derive(Default)]
struct PaneAlertStaging {
    active: Option<ActivePaneAlerts>,
    dispatch: Arc<crate::PaneAlertDispatchQueue>,
}

struct ActivePaneAlerts {
    alerts: Vec<Alert>,
    remaining_text_bytes: usize,
}

struct PaneAlertCompletion {
    delivered: bool,
}

#[cfg(test)]
static ALERT_DELIVERY_CANCELLED: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
static ALERT_MAIN_HANDOFFS: AtomicUsize = AtomicUsize::new(0);

impl Drop for PaneAlertCompletion {
    fn drop(&mut self) {
        if !self.delivered {
            metrics::counter!("mux.pane_alerts.delivery_cancelled").increment(1);
            #[cfg(test)]
            ALERT_DELIVERY_CANCELLED.fetch_add(1, Ordering::Release);
        }
    }
}

struct FundedPaneAlerts {
    dispatch: Option<crate::FundedPaneAlertDispatch>,
    output: Option<crate::PaneAlertOutput>,
    alerts: Vec<Alert>,
    text_bytes: usize,
    terminal: std::sync::Weak<Mutex<Terminal>>,
    historical: Option<crate::AdmittedHistoricalAlerts>,
}

struct AdmittedPaneActions {
    actions: Vec<Action>,
    alerts: Option<FundedPaneAlerts>,
    staging: Arc<Mutex<PaneAlertStaging>>,
}

fn background_alert_refusal(
    error: promise::spawn::BackgroundSpawnError,
) -> PaneActionAdmissionRefusal {
    use promise::spawn::BackgroundSpawnError;
    match error {
        BackgroundSpawnError::TaskCapacityExhausted { .. } => PaneActionAdmissionRefusal::Capacity,
        BackgroundSpawnError::EstimatedByteCapacityExhausted {
            requested,
            capacity,
            ..
        } if requested <= capacity => PaneActionAdmissionRefusal::Capacity,
        BackgroundSpawnError::EstimatedByteCapacityExhausted { .. }
        | BackgroundSpawnError::ZeroEstimatedBytes => PaneActionAdmissionRefusal::SizeOverflow,
        BackgroundSpawnError::WorkerUnavailable(_) => {
            PaneActionAdmissionRefusal::SchedulerUnavailable
        }
    }
}

impl FundedPaneAlerts {
    fn reserve(
        preflight: PaneAlertPreflight,
        output: &mut Option<crate::PaneAlertOutput>,
        terminal: &Arc<Mutex<Terminal>>,
    ) -> Result<Option<Self>, PaneActionAdmissionRefusal> {
        if output.is_none() {
            return Ok(None);
        }
        let bytes = preflight
            .retained_bytes()
            .ok_or(PaneActionAdmissionRefusal::SizeOverflow)?;
        if !promise::spawn::is_scheduler_configured() {
            return Err(PaneActionAdmissionRefusal::SchedulerUnavailable);
        }
        let historical = output
            .as_ref()
            .expect("checked output authority")
            .reserve_historical(&preflight.historical)?;
        let bytes = bytes
            .checked_add(historical.retained_bytes())
            .ok_or(PaneActionAdmissionRefusal::SizeOverflow)?;
        historical.check_dispatch_fit(bytes)?;
        let dispatch = crate::FundedPaneAlertDispatch::reserve(bytes)?;
        let mut alerts = Vec::new();
        alerts
            .try_reserve_exact(preflight.count)
            .map_err(|_| PaneActionAdmissionRefusal::Allocation)?;
        // Do not assume an allocator's capacity rounding is free.
        if alerts.capacity() > preflight.count {
            return Err(PaneActionAdmissionRefusal::Allocation);
        }
        Ok(Some(Self {
            dispatch: Some(dispatch),
            output: output.take(),
            alerts,
            text_bytes: preflight.text_bytes,
            terminal: Arc::downgrade(terminal),
            historical: Some(historical),
        }))
    }

    fn publish(mut self, queue: &crate::PaneAlertDispatchQueue) {
        let dispatch = self
            .dispatch
            .take()
            .expect("funded batch owns dispatch admission");
        let output = self
            .output
            .take()
            .expect("funded batch owns output authority");
        let alerts = std::mem::take(&mut self.alerts);
        let terminal = self.terminal.clone();
        let mut historical = self.historical.take().expect("funded historical receipts");
        let mut completion = PaneAlertCompletion { delivered: false };
        dispatch.publish(
            queue,
            async move {
                wait_for_alert_terminal_unlock(&terminal).await;
                metrics::counter!("mux.pane_alerts.main_handoff").increment(1);
                #[cfg(test)]
                ALERT_MAIN_HANDOFFS.fetch_add(1, Ordering::Release);
            },
            async move {
                for alert in alerts {
                    if crate::HistoricalAlertDemand::for_alert(&alert).is_some()
                        && !output.deliver_historical(&mut historical, &alert)
                    {
                        return;
                    }
                    if !output.dispatch_alert(alert) {
                        return;
                    }
                    historical.finish_delivered().await;
                }
                historical.finish().await;
                drop(output);
                completion.delivered = true;
                // Keep the whole guard in this callback until delivery ends;
                // capturing only the flag would release its FIFO successor early.
                drop(completion);
            },
        );
    }
}

async fn wait_for_alert_terminal_unlock(terminal: &std::sync::Weak<Mutex<Terminal>>) {
    loop {
        let Some(terminal) = terminal.upgrade() else {
            return;
        };
        if terminal.try_lock().is_some() {
            return;
        }
        drop(terminal);
        promise::spawn::sleep(Duration::from_millis(1)).await;
    }
}

impl Drop for FundedPaneAlerts {
    fn drop(&mut self) {
        let Some(dispatch) = self.dispatch.take() else {
            return;
        };
        let output = self.output.take();
        let historical = self.historical.take();
        let alerts = std::mem::take(&mut self.alerts);
        let terminal = self.terminal.clone();
        // The global background executor has no main-binding close path and
        // never polls inline. Even cancellation of an unapplied ring entry
        // must not release its lifecycle continuation under a terminal/tab lock.
        dispatch.retire_after(
            async move {
                wait_for_alert_terminal_unlock(&terminal).await;
            },
            (output, historical, alerts),
        );
    }
}

fn normalize_alert_text(alert: &mut Alert) -> usize {
    fn normalize(text: &mut String) -> usize {
        *text = std::mem::take(text).into_boxed_str().into_string();
        text.capacity()
    }
    match alert {
        Alert::ToastNotification { title, body, .. } => {
            title.as_mut().map_or(0, normalize) + normalize(body)
        }
        Alert::IconTitleChanged(title) | Alert::TabTitleChanged(title) => {
            title.as_mut().map_or(0, normalize)
        }
        Alert::WindowTitleChanged(title) => normalize(title),
        Alert::SetUserVar { name, value } => normalize(name) + normalize(value),
        Alert::SetProfileRequested { name } => normalize(name),
        Alert::MouseShapeRequested { shape } => normalize(shape),
        Alert::ImageAltText { text, .. } => normalize(text),
        Alert::Bell
        | Alert::CurrentWorkingDirectoryChanged
        | Alert::PaletteChanged
        | Alert::OutputSinceFocusLost
        | Alert::Progress(_) => 0,
    }
}

impl AdmittedPaneActions {
    fn apply(self, terminal: &mut Terminal) {
        let Self {
            actions,
            mut alerts,
            staging,
        } = self;
        {
            let mut state = staging.lock();
            assert!(
                state.active.is_none(),
                "terminal action batches are serialized"
            );
            state.active = alerts.as_mut().map(|batch| ActivePaneAlerts {
                alerts: std::mem::take(&mut batch.alerts),
                remaining_text_bytes: batch.text_bytes,
            });
        }
        let application = PaneAlertApplication { alerts, staging };
        terminal.perform_actions(actions);
        drop(application);
    }
}

struct PaneAlertApplication {
    alerts: Option<FundedPaneAlerts>,
    staging: Arc<Mutex<PaneAlertStaging>>,
}

impl Drop for PaneAlertApplication {
    fn drop(&mut self) {
        let Some(mut batch) = self.alerts.take() else {
            return;
        };
        let mut state = self.staging.lock();
        batch.alerts = state
            .active
            .take()
            .expect("funded alert staging remains installed")
            .alerts;
        if batch.alerts.is_empty() {
            // Ordinary output often changes only the terminal model. It
            // needs no historical callback or FIFO node. Preserve the last
            // nonempty tail; funded Drop releases output custody off-thread.
            drop(state);
            return;
        }
        let dispatch = Arc::clone(&state.dispatch);
        // Link the successor before releasing staging authority. Scheduling
        // consumes prepaid credits and never waits for terminal or GUI work.
        batch.publish(&dispatch);
        drop(state);
    }
}

impl AlertHandler for LocalPaneNotifHandler {
    fn alert(&mut self, mut alert: Alert) {
        let mut state = self.staging.lock();
        let Some(active) = state.active.as_mut() else {
            // Unregistered model-only batches never acquire authority midway
            // through application, even if registration races this callback.
            return;
        };
        let bytes = normalize_alert_text(&mut alert);
        active.remaining_text_bytes = active
            .remaining_text_bytes
            .checked_sub(bytes)
            .expect("alert preflight covers retained strings");
        assert!(
            active.alerts.len() < active.alerts.capacity(),
            "alert preflight covers event count"
        );
        active.alerts.push(alert);
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
    fn prepare_alert_output(
        &self,
    ) -> Result<Option<crate::PaneAlertOutput>, PaneActionAdmissionRefusal> {
        let Some(registration) = self.mux_registration.load() else {
            return Ok(None);
        };
        if !promise::spawn::is_scheduler_configured() {
            return Err(PaneActionAdmissionRefusal::SchedulerUnavailable);
        }
        // Initialize the lazy executor/reactor before taking any terminal lock.
        // Actual batch admission later is a bounded, nonblocking counter update.
        drop(promise::spawn::try_reserve_background_task(1).map_err(background_alert_refusal)?);
        registration
            .reserve_alert_output()
            .map(Some)
            .ok_or(PaneActionAdmissionRefusal::Retired)
    }

    fn admit_alert_actions(
        &self,
        actions: &mut Vec<Action>,
        pending_tmux_title_bytes: usize,
        output: &mut Option<crate::PaneAlertOutput>,
    ) -> Result<AdmittedPaneActions, PaneActionAdmissionRefusal> {
        let preflight = PaneAlertPreflight::for_actions(actions, pending_tmux_title_bytes)
            .ok_or(PaneActionAdmissionRefusal::SizeOverflow)?;
        let alerts = FundedPaneAlerts::reserve(preflight, output, &self.terminal)?;
        Ok(AdmittedPaneActions {
            actions: std::mem::take(actions),
            alerts,
            staging: Arc::clone(&self.alert_staging),
        })
    }

    /// Capture a legacy mux-owned pane terminal checkpoint using distinct model-only authority.
    ///
    /// The capture separates hot-state capture from cold-history materialization:
    /// 1. Under terminal lock: drain/verify pending actions, capture bounded hot state, and pin
    ///    cold-history generation from the scrollback spill sink.
    /// 2. Release terminal lock immediately.
    /// 3. Outside terminal lock: materialize cold-history rows from the spill sink, revalidate
    ///    generation freshness, and assemble canonical `RecoveryTerminalCheckpointV3`.
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
        let mut output = if pending_actions.is_empty() {
            None
        } else {
            self.prepare_alert_output()
                .map_err(LegacyTerminalCaptureError::ActionAdmission)?
        };
        let _output_application = self.output_application.lock();
        let mut terminal = self.locked_terminal();
        if !pending_actions.is_empty() {
            let pending_title = terminal.pending_tmux_title_bytes();
            self.admit_alert_actions(pending_actions, pending_title, &mut output)
                .map_err(LegacyTerminalCaptureError::ActionAdmission)?
                .apply(&mut terminal);
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
    ) -> Result<RecoveryTerminalCheckpointV3, LegacyTerminalCaptureError> {
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
    ) -> Result<RecoveryTerminalCheckpointV3, LegacyTerminalCaptureError> {
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

        let mut output = if pending_actions.is_empty() {
            None
        } else {
            self.prepare_alert_output()
                .map_err(LegacyTerminalCaptureError::ActionAdmission)?
        };
        // 1. Under terminal lock: drain/apply actions, capture hot state, pin cold generation
        let staged = {
            let _output_application = self.output_application.lock();
            let mut terminal = self.locked_terminal();
            if policy == PendingActionDrainPolicy::DrainAndApply {
                let pending_title = terminal.pending_tmux_title_bytes();
                self.admit_alert_actions(pending_actions, pending_title, &mut output)
                    .map_err(LegacyTerminalCaptureError::ActionAdmission)?
                    .apply(&mut terminal);
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

        // 3. Serialize canonical payload and assemble RecoveryTerminalCheckpointV3 outside terminal lock
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
            .ok_or_else(|| metadata_busy(MetadataRefusalStage::LayoutObservation))?;
        let Some(source_changed) = term
            .screen_mut()
            .refresh_cold_source_observation()
            .inspect_err(|_| {
                record_metadata_refusal(MetadataRefusalStage::SinkInterval);
            })?
        else {
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

    /// Publish a native coordinate action without accepting a reflow transition.
    /// Unlike the remote layout protocol, an initial identity mapping can be
    /// installed here: the callback receives its new authority atomically.
    /// The callback must not call pane methods that acquire the terminal lock.
    pub fn publish_line_reads_at_unchanged_coordinates(
        &self,
        reads: &[frankenterm_term::screen::ScreenLineRead],
        expected_floor: SequenceNo,
        expected_sequence: SequenceNo,
        expected_dimensions: RenderableDimensions,
        publish: &mut dyn FnMut(SequenceNo, SequenceNo, RenderableDimensions),
    ) -> Result<bool, frankenterm_term::screen::ColdReadMetadataBusy> {
        let _diagnostic = MetadataRefusalDiagnostic::new();
        let mut term = self
            .terminal
            .try_lock()
            .ok_or_else(|| metadata_busy(MetadataRefusalStage::PublishLayoutTerminal))?;
        let Some(floor) =
            Self::refresh_line_layout_floor(&self.line_layout_observation, &mut term)?
        else {
            return Ok(false);
        };
        let Some(dimensions) = terminal_try_get_dimensions(&mut term) else {
            return Ok(false);
        };
        if expected_sequence == SequenceNo::MAX
            || floor != expected_floor
            || term.current_seqno() != expected_sequence
            || dimensions != expected_dimensions
        {
            record_metadata_refusal(MetadataRefusalStage::PublicationGeometry);
            return Ok(false);
        }
        for read in reads {
            if !term.screen().try_validate_line_read(read)?
                || !term.screen().line_read_preserves_coordinates(read)
            {
                return Ok(false);
            }
        }
        let changed = reads
            .iter()
            .any(|read| term.screen().line_read_changes_layout(read));
        let Some(published_sequence) = expected_sequence
            .checked_add(usize::from(changed))
            .filter(|sequence| *sequence != SequenceNo::MAX)
        else {
            return Ok(false);
        };
        if changed {
            term.increment_seqno();
        }
        for read in reads {
            term.screen_mut()
                .install_line_read_layout(read, published_sequence);
        }
        // A storage observation can advance independently of the terminal.
        // Do not authorize an unrelated prune/replacement as our own install.
        // Once installation happened, unavailable authority is a refusal, not
        // permission to retry the old numeric request with a newer sequence.
        let Ok(Some(published_floor)) =
            Self::refresh_line_layout_floor(&self.line_layout_observation, &mut term)
        else {
            return Ok(false);
        };
        let Some(published_dimensions) = terminal_try_get_dimensions(&mut term) else {
            return Ok(false);
        };
        if term.current_seqno() != published_sequence || published_dimensions != expected_dimensions
        {
            return Ok(false);
        }
        publish(published_floor, published_sequence, published_dimensions);
        Ok(true)
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
                .selection_points_resident_span_unchanged_since(&points, expected_sequence)
            {
                return Err(SelectionAnchorCaptureError::SourceChanged);
            }
        }
        if points
            .iter()
            .flatten()
            .any(|point| point.row < dimensions.scrollback_top)
        {
            return Ok(None);
        }
        let request = ColdSelectionRequest::Capture {
            floor: expected_floor,
            sequence: expected_sequence,
            dimensions: expected_dimensions,
            points,
        };
        match self.cold_selection_ready(&request, floor, sequence, dimensions, &term) {
            Some(ColdSelectionValue::Captured(value)) => return value,
            Some(ColdSelectionValue::Busy) => {
                if let Some(registration) = self.mux_registration.load() {
                    retry_cold_viewport(registration, Arc::clone(&self.cold_selection_retry));
                }
                return Err(SelectionAnchorCaptureError::Busy);
            }
            _ => {}
        }
        let ranges = term
            .screen()
            .selection_points_read_ranges(&points)
            .map_err(|_| {
                if let Some(registration) = self.mux_registration.load() {
                    retry_cold_viewport(registration, Arc::clone(&self.cold_selection_retry));
                }
                SelectionAnchorCaptureError::Busy
            })?;
        // A presented cold frame commonly already owns the endpoint group.
        if ranges.len() > 3 {
            return Err(SelectionAnchorCaptureError::SourceChanged);
        }
        // Reuse it only under the exact pane and Screen read authority.
        if let Some(registration) = self.mux_registration.load() {
            if let Some(cache) = COLD_VIEWPORT_CACHE.try_lock() {
                let reads: Vec<_> = cache
                    .iter()
                    .filter(|entry| entry.registration == registration.wire_identity())
                    .map(|entry| entry.read.as_ref())
                    .collect();
                if let Ok(Some(anchor)) = term
                    .screen_mut()
                    .capture_selection_anchor_with_reads(sequence, points, &reads)
                {
                    return Ok(Some(anchor));
                }
            }
        }
        if ranges.is_empty() {
            return term
                .screen_mut()
                .capture_selection_anchor_with_reads(sequence, points, &[])
                .map_err(|_| {
                    if let Some(registration) = self.mux_registration.load() {
                        retry_cold_viewport(registration, Arc::clone(&self.cold_selection_retry));
                    }
                    SelectionAnchorCaptureError::Busy
                });
        }
        self.request_cold_selection(&mut term, request, ranges);
        Err(SelectionAnchorCaptureError::Busy)
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
        let request = ColdSelectionRequest::Resolve(anchor.downgrade());
        match self.cold_selection_ready(&request, floor, sequence, dimensions, &term) {
            Some(ColdSelectionValue::Resolved(points)) => {
                return Some((floor, sequence, dimensions, points))
            }
            Some(ColdSelectionValue::Busy) => {
                if let Some(registration) = self.mux_registration.load() {
                    retry_cold_viewport(registration, Arc::clone(&self.cold_selection_retry));
                }
                return None;
            }
            _ => {}
        }
        let ranges = match term.screen().selection_anchor_read_ranges(anchor, sequence) {
            Ok(Some(ranges)) => ranges,
            Ok(None) => return Some((floor, sequence, dimensions, None)),
            Err(_) => {
                if let Some(registration) = self.mux_registration.load() {
                    retry_cold_viewport(registration, Arc::clone(&self.cold_selection_retry));
                }
                return None;
            }
        };
        if ranges.len() > 3 {
            return Some((floor, sequence, dimensions, None));
        }
        if let Some(registration) = self.mux_registration.load() {
            if let Some(cache) = COLD_VIEWPORT_CACHE.try_lock() {
                let reads: Vec<_> = cache
                    .iter()
                    .filter(|entry| entry.registration == registration.wire_identity())
                    .map(|entry| entry.read.as_ref())
                    .collect();
                if let Ok(Some(points)) = term
                    .screen()
                    .resolve_selection_anchor_with_reads(anchor, sequence, &reads)
                {
                    return Some((floor, sequence, dimensions, Some(points)));
                }
            }
        }
        if ranges.is_empty() {
            return match term
                .screen()
                .resolve_selection_anchor_with_reads(anchor, sequence, &[])
            {
                Ok(points) => Some((floor, sequence, dimensions, points)),
                Err(_) => {
                    if let Some(registration) = self.mux_registration.load() {
                        retry_cold_viewport(registration, Arc::clone(&self.cold_selection_retry));
                    }
                    None
                }
            };
        }
        self.request_cold_selection(&mut term, request, ranges);
        None
    }

    fn cold_selection_ready(
        &self,
        request: &ColdSelectionRequest,
        floor: SequenceNo,
        sequence: SequenceNo,
        dimensions: RenderableDimensions,
        term: &Terminal,
    ) -> Option<ColdSelectionValue> {
        let mut slot = self.cold_selection.try_lock()?;
        let index = slot.iter().position(|work| work.request == *request)?;
        let work = &mut slot[index];
        work.renewed = Instant::now();
        let ready = work.ready.as_ref()?;
        if work.request != *request
            || work.cancelled.load(Ordering::Acquire)
            || ready.floor != floor
            || ready.sequence > sequence
            || ready.dimensions != dimensions
        {
            return None;
        }
        if let ColdSelectionValue::RetryAt(retry_at) = &ready.value {
            return (Instant::now() < *retry_at).then_some(ColdSelectionValue::Busy);
        }
        let anchor = match (&ready.value, request) {
            (ColdSelectionValue::Captured(Ok(Some(anchor))), _) => Some(anchor.clone()),
            (ColdSelectionValue::Resolved(Some(_)), ColdSelectionRequest::Resolve(anchor)) => {
                anchor.upgrade()
            }
            _ => None,
        };
        if let Some(anchor) = anchor {
            // A sink can revoke retained identity without advancing terminal
            // sequence. Revalidate even an exact-sequence cache hit. Conversely,
            // unrelated output alone must not force another payload hydration.
            return Some(
                match term.screen().selection_anchor_read_ranges(&anchor, sequence) {
                    Ok(Some(_)) => {
                        let value = ready.value.clone();
                        if matches!(value, ColdSelectionValue::Captured(Ok(Some(_)))) {
                            slot.remove(index);
                        }
                        value
                    }
                    Err(_) => ColdSelectionValue::Busy,
                    Ok(None) => match request {
                        ColdSelectionRequest::Capture { .. } => ColdSelectionValue::Captured(Err(
                            SelectionAnchorCaptureError::SourceChanged,
                        )),
                        ColdSelectionRequest::Resolve(_) => ColdSelectionValue::Resolved(None),
                    },
                },
            );
        }
        let value = (ready.sequence == sequence).then(|| ready.value.clone());
        if matches!(value, Some(ColdSelectionValue::Captured(Ok(Some(_))))) {
            slot.remove(index);
        }
        value
    }

    fn request_cold_selection(
        &self,
        term: &mut Terminal,
        request: ColdSelectionRequest,
        ranges: Vec<Range<StableRowIndex>>,
    ) {
        if ranges.is_empty() || ranges.len() > 3 {
            return;
        }
        let Some(registration) = self.mux_registration.load() else {
            return;
        };
        let retry = Arc::clone(&self.cold_selection_retry);
        let Some(mut slot) = self.cold_selection.try_lock() else {
            retry_cold_viewport(registration, retry);
            return;
        };
        let now = Instant::now();
        slot.retain(|work| {
            let retain = now.duration_since(work.renewed) < COLD_SELECTION_WORK_LEASE;
            if !retain {
                work.cancelled.store(true, Ordering::Release);
            }
            retain
        });
        if let Some(work) = slot.iter_mut().find(|work| work.request == request) {
            work.renewed = now;
            if work.ready.is_none() {
                return;
            }
        }
        // This request's obsolete completion can be replaced, but another
        // live request must retain its progress even when polled alternately.
        slot.retain(|work| {
            if work.request == request {
                work.cancelled.store(true, Ordering::Release);
                false
            } else {
                true
            }
        });
        if slot.len() >= MAX_COLD_SELECTION_WORK {
            drop(slot);
            retry_cold_viewport(registration, retry);
            return;
        }
        let Some(permit) = crate::pane::LineReadPermit::try_acquire() else {
            drop(slot);
            retry_cold_viewport(registration, retry);
            return;
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        slot.push(ColdSelectionWork {
            request: request.clone(),
            cancelled: Arc::clone(&cancelled),
            ready: None,
            renewed: now,
        });
        drop(slot);
        let completion = ColdSelectionCompletion {
            slot: Arc::clone(&self.cold_selection),
            cancelled: Arc::clone(&cancelled),
        };
        let terminal = Arc::clone(&self.terminal);
        let layout = Arc::clone(&self.line_layout_observation);
        let worker_registration = registration.clone();
        let worker_retry = Arc::clone(&retry);
        let worker_cancelled = Arc::clone(&cancelled);
        let capture_aborted = Arc::new(AtomicBool::new(false));
        let abort_for_worker = Arc::clone(&capture_aborted);
        let abort_for_completion = Arc::clone(&capture_aborted);
        let captured_witnesses = Arc::new(Mutex::new(Vec::<
            frankenterm_term::screen::LineReadFailureWitness,
        >::with_capacity(ranges.len())));
        let worker_witnesses = Arc::clone(&captured_witnesses);
        let worker = permit.start(
            move || {
                worker_cancelled.load(Ordering::Acquire) || abort_for_worker.load(Ordering::Acquire)
            },
            move |result, _permit| {
                if abort_for_completion.load(Ordering::Acquire) {
                    drop(completion);
                    drop(result);
                    // A caller-side wake can run before this worker retires its
                    // slot. Always retain a wake after retirement as well.
                    retry_cold_viewport(worker_registration, worker_retry);
                    return;
                }
                // Registry mutation and completion publication happen under the
                // exact live pane lease; no main-thread payload cache is needed.
                let published = worker_registration
                    .try_with_current(|pane| {
                        let Some(mut term) = terminal.try_lock() else {
                            return false;
                        };
                        let Some(mut slot) = completion.slot.try_lock() else {
                            return false;
                        };
                        let Some(work) = slot.iter_mut().find(|work| {
                            Arc::ptr_eq(&work.cancelled, &completion.cancelled)
                                && !work.cancelled.load(Ordering::Acquire)
                        }) else {
                            return true;
                        };
                        let Some(floor) = Self::refresh_line_layout_floor(&layout, &mut term)
                            .ok()
                            .flatten()
                        else {
                            return false;
                        };
                        let Some(dimensions) = terminal_try_get_dimensions(&mut term) else {
                            return false;
                        };
                        let sequence = term.current_seqno();
                        let invalid = match &request {
                            ColdSelectionRequest::Capture { .. } => ColdSelectionValue::Captured(
                                Err(SelectionAnchorCaptureError::SourceChanged),
                            ),
                            ColdSelectionRequest::Resolve(_) => ColdSelectionValue::Resolved(None),
                        };
                        let capture_current = match &request {
                            ColdSelectionRequest::Capture {
                                floor: expected_floor,
                                sequence: expected_sequence,
                                dimensions: expected_dimensions,
                                points,
                            } => {
                                floor == *expected_floor
                                    && dimensions == *expected_dimensions
                                    && sequence >= *expected_sequence
                                    && term
                                        .screen()
                                        .selection_points_resident_span_unchanged_since(
                                            points,
                                            *expected_sequence,
                                        )
                            }
                            ColdSelectionRequest::Resolve(_) => true,
                        };
                        let value = if !capture_current {
                            invalid
                        } else {
                            let reads = match &result {
                                Ok(reads) => reads,
                                Err(error)
                                    if error
                                        .is::<frankenterm_term::screen::ColdReadMetadataBusy>()
                                        || error.is::<
                                            frankenterm_term::screen::ColdReadGeometryUnavailable,
                                        >() =>
                                {
                                    // Primary resize geometry can be visible before
                                    // the cold index is published. Its temporary
                                    // absence does not revoke the selected text.
                                    return false
                                }
                                Err(_) => {
                                    // An off-thread read failed against its captured
                                    // source, not necessarily the source now locked.
                                    // Never stamp an obsolete failure with current
                                    // authority and permanently clear its selection.
                                    let witnesses = worker_witnesses.lock();
                                    if witnesses.is_empty()
                                        || witnesses
                                            .iter()
                                            .any(|witness| !witness.matches(term.screen()))
                                    {
                                        return false;
                                    }
                                    drop(witnesses);
                                    work.ready = Some(ColdSelectionReady {
                                        floor,
                                        sequence,
                                        dimensions,
                                        // Matching retained identity is not evidence
                                        // of pruning: storage may refuse one read.
                                        value: ColdSelectionValue::RetryAt(
                                            Instant::now() + Duration::from_millis(100),
                                        ),
                                    });
                                    drop(slot);
                                    drop(term);
                                    pane.notify_lines_ready();
                                    retry_cold_viewport(
                                        worker_registration.clone(),
                                        Arc::clone(&worker_retry),
                                    );
                                    return true;
                                }
                            };
                            for read in reads {
                                match term.screen().try_validate_line_read(read) {
                                    Ok(true) => {}
                                    Ok(false) | Err(_) => return false,
                                }
                            }
                            if reads
                                .iter()
                                .any(|read| term.screen().line_read_changes_layout(read))
                            {
                                if sequence == SequenceNo::MAX {
                                    return false;
                                }
                                term.increment_seqno();
                                let published_sequence = term.current_seqno();
                                for read in reads {
                                    term.screen_mut()
                                        .install_line_read_layout(read, published_sequence);
                                }
                                // The refresh proves layout, not the original numeric
                                // gesture. Retry under newly observed authority before
                                // capturing endpoints or resolving the actual groups.
                                return false;
                            }
                            let refs: Vec<_> = reads.iter().collect();
                            match &request {
                                ColdSelectionRequest::Capture { points, .. } => {
                                    let Ok(anchor) =
                                        term.screen_mut().capture_selection_anchor_with_reads(
                                            sequence, *points, &refs,
                                        )
                                    else {
                                        return false;
                                    };
                                    ColdSelectionValue::Captured(Ok(anchor))
                                }
                                ColdSelectionRequest::Resolve(anchor) => {
                                    let Some(anchor) = anchor.upgrade() else {
                                        return true;
                                    };
                                    let Ok(points) =
                                        term.screen().resolve_selection_anchor_with_reads(
                                            &anchor, sequence, &refs,
                                        )
                                    else {
                                        return false;
                                    };
                                    if points.is_none() {
                                        match term
                                            .screen()
                                            .selection_anchor_read_ranges(&anchor, sequence)
                                        {
                                            Ok(None) => {}
                                            // A bounded payload fallback can be
                                            // valid without a complete layout
                                            // projection. Retained source is not
                                            // permanently invalid just because
                                            // this read cannot map its points.
                                            Ok(Some(_)) | Err(_) => return false,
                                        }
                                    }
                                    ColdSelectionValue::Resolved(points)
                                }
                            }
                        };
                        work.ready = Some(ColdSelectionReady {
                            floor,
                            sequence,
                            dimensions,
                            value,
                        });
                        drop(slot);
                        drop(term);
                        pane.notify_lines_ready();
                        true
                    })
                    .unwrap_or(true);
                drop(completion);
                // All decoded reads are destroyed on this blocking worker. The
                // shared permit bounds combined payload across all three groups.
                drop(result);
                if !published {
                    retry_cold_viewport(worker_registration, worker_retry);
                }
            },
        );
        let Ok(worker) = worker else {
            retry_cold_viewport(registration, retry);
            return;
        };
        let mut budget = frankenterm_term::screen::LineReadCaptureBudget::default();
        let mut plans = Vec::with_capacity(ranges.len());
        let mut capture_error = None;
        for range in ranges {
            match term
                .screen()
                .capture_line_read_with_budget(range, &mut budget)
            {
                Ok(plan) => {
                    captured_witnesses.lock().push(plan.failure_witness());
                    plans.push(plan);
                }
                Err(error) => {
                    capture_error = Some(error);
                    break;
                }
            }
        }
        match capture_error {
            None => worker.submit(plans),
            Some(error) => {
                if !error.is::<frankenterm_term::screen::ColdReadMetadataBusy>()
                    && !error.is::<frankenterm_term::screen::ColdReadGeometryUnavailable>()
                {
                    if let Some(floor) =
                        Self::refresh_line_layout_floor(&self.line_layout_observation, term)
                            .ok()
                            .flatten()
                    {
                        if let Some(dimensions) = terminal_try_get_dimensions(term) {
                            if let Some(mut slot) = self.cold_selection.try_lock() {
                                if let Some(work) = slot
                                    .iter_mut()
                                    .find(|work| Arc::ptr_eq(&work.cancelled, &cancelled))
                                {
                                    work.ready = Some(ColdSelectionReady {
                                        floor,
                                        sequence: term.current_seqno(),
                                        dimensions,
                                        value: ColdSelectionValue::RetryAt(
                                            Instant::now() + Duration::from_millis(100),
                                        ),
                                    });
                                }
                            }
                        }
                    }
                }
                // Earlier plans may already own resident payload. Hand every
                // captured plan to the admitted worker even when a later
                // capture failed; cancellation prevents hydration there.
                capture_aborted.store(true, Ordering::Release);
                worker.submit(plans);
                retry_cold_viewport(registration, retry);
            }
        }
    }

    pub fn try_capture_render_frame(
        &self,
        mut viewport: Option<NativeViewport>,
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
        let sequence = term.current_seqno();
        if let Some(viewport) = viewport.as_mut() {
            if let Some(anchor) = viewport.anchor.as_ref() {
                match term.screen().resolve_selection_anchor(anchor, sequence) {
                    Some([Some(point), _, _]) => viewport.row = point.row,
                    _ => viewport.anchor = None,
                }
            }
            if let Some(anchor) = viewport.cold_anchor.as_ref() {
                if let Some(requested) =
                    term.screen().cold_viewport_anchor_read_range(anchor).ok()?
                {
                    let registration = self.mux_registration.load()?;
                    let resolved = COLD_VIEWPORT_CACHE.try_lock()?.iter().find_map(|entry| {
                        if entry.registration != registration.wire_identity()
                            || !term.screen().validates_line_read(&entry.read)
                        {
                            return None;
                        }
                        entry.read.resolve_viewport_anchor(anchor)
                    });
                    if let Some(row) = resolved {
                        viewport.row = row;
                    } else {
                        // Hydration is worker-owned and must never run under
                        // terminal ownership. Retain the original anchor while
                        // the new-width group is fetched; do not paint its old
                        // numeric row in the meantime. A stale layout first
                        // requests a bounded geometry refresh; the next capture
                        // then requests the anchor's actual logical group.
                        let intent = ColdViewportIntent {
                            anchor: anchor.clone(),
                            dimensions,
                        };
                        drop(term);
                        self.cold_viewport_lines(requested, Some(intent));
                        return None;
                    }
                } else {
                    viewport.cold_anchor = None;
                }
            }
        }
        // Scrollback eviction and clearing can invalidate a stored viewport
        // without a GUI scroll event. Normalize against this same observation;
        // requesting an evicted row would otherwise retry hydration forever.
        let first = viewport
            .as_ref()
            .map(|viewport| viewport.row)
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
            viewport: None,
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
        if first < dimensions.physical_top {
            let mut viewport = viewport.unwrap_or_else(|| NativeViewport::new(first));
            if viewport.row != first {
                viewport.anchor = None;
                viewport.cold_anchor = None;
            }
            viewport.row = first;
            if viewport.anchor.is_none() {
                viewport.anchor = term.screen_mut().capture_selection_anchor(
                    frame.source_sequence,
                    [
                        Some(frankenterm_term::screen::SelectionAnchorCoordinate {
                            row: first,
                            column: Some(0),
                        }),
                        None,
                        None,
                    ],
                );
            }
            if cold && viewport.cold_anchor.is_none() {
                let registration = self.mux_registration.load()?;
                viewport.cold_anchor = COLD_VIEWPORT_CACHE.try_lock()?.iter().find_map(|entry| {
                    if entry.registration != registration.wire_identity()
                        || !term.screen().validates_line_read(&entry.read)
                    {
                        return None;
                    }
                    entry.read.capture_viewport_anchor(first)
                });
            }
            frame.viewport = Some(viewport);
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

    fn cold_viewport_lines(
        &self,
        requested: Range<StableRowIndex>,
        intent: Option<ColdViewportIntent>,
    ) -> (StableRowIndex, Vec<Line>) {
        let empty = || (requested.start, Vec::new());
        if requested.end.saturating_sub(requested.start).max(0) as usize
            > frankenterm_term::screen::ScreenLineRead::MAX_ROWS
        {
            return empty();
        }
        let Some(registration) = self.mux_registration.load() else {
            return empty();
        };
        let failure = self
            .cold_viewport_failure
            .try_lock()
            .and_then(|failure| failure.clone());
        if let Some(failure) = failure {
            if failure.requested == requested && Instant::now() < failure.retry_at {
                if let Some(term) = self.terminal.try_lock() {
                    if failure.witness.matches(term.screen()) {
                        retry_cold_viewport(
                            registration.clone(),
                            Arc::clone(&self.cold_viewport_retry),
                        );
                        return empty();
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
        let completion = Arc::new(ColdViewportCompletion {
            state: Arc::clone(&self.cold_viewport_pending),
            cancelled: Arc::clone(&cancelled),
        });
        let mut response_range = requested.clone();
        let retry = Arc::clone(&self.cold_viewport_retry);
        let capture_registration = registration.clone();
        let capture_retry = Arc::clone(&retry);
        let worker_cancelled = Arc::clone(&cancelled);
        let worker = permit.start(move || worker_cancelled.load(Ordering::Acquire), move |mut result, permit| {
            let mut required_layout = None;
            // One anchor read and at most one visible-context read, charged to
            // the same worker. No GUI capture is needed between publications.
            for stage in 0..2 {
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
                        // Validate outside the failure lock: publication takes
                        // terminal authority separately. Retain one diagnostic
                        // per unchanged source, while payload retries remain live.
                        let previous = failure_state.lock().clone();
                        let repeated = terminal_for_failure.upgrade().is_some_and(|terminal| {
                            let Some(term) = terminal.try_lock() else { return false; };
                            previous.as_ref().is_some_and(|previous| {
                                previous.requested == response_range
                                    && previous.witness.matches(term.screen())
                            })
                        });
                        *failure_state.lock() = Some(ColdViewportFailure { requested: response_range.clone(), witness: failure_witness.clone(), retry_at: Instant::now() + Duration::from_millis(100) });
                        retry_cold_viewport(registration.clone(), Arc::clone(&retry));
                        if !repeated { schedule_local_pane_main_thread(
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
                        ); }
                    }
                    return;
                }
            };
            let Some(read) = plans.pop() else { return; };
            let (sender, retired) = sync_channel(1);
            let retirement = ColdViewportRetirement { read: Some(Arc::new(read)), evicted: Vec::with_capacity(1), sender, followup: None };
            let publish_registration = registration.clone();
            let publish_retry = Arc::clone(&retry);
            let publish_completion = Arc::clone(&completion);
            let publish_failure = Arc::clone(&failure_state);
            let publish_range = response_range.clone();
            let publish_intent = if stage == 0 { intent.clone() } else { None };
            let publish_terminal = terminal_for_failure.clone();
            schedule_local_pane_main_thread(
                promise::spawn::MainThreadServiceClass::Interactive,
                LOCAL_PANE_MAIN_THREAD_ESTIMATED_BYTES,
                "cold_viewport_publish",
                || async move {
                    let registration = publish_registration;
                    let retry = publish_retry;
                    let completion = publish_completion;
                    let failure_state = publish_failure;
                    let mut retirement = retirement;
                    if completion.cancelled.load(Ordering::Acquire) {
                        retry_cold_viewport(registration, retry);
                        return;
                    }
                    if let Some(read) = retirement.read.as_ref().map(Arc::clone) {
                            let _ = registration.try_with_current(|pane| {
                                let Some(mut pending) = completion.state.try_lock() else {
                                    retry_cold_viewport(registration.clone(), Arc::clone(&retry));
                                    return;
                                };
                                if !pending.as_ref().is_some_and(|pending| Arc::ptr_eq(&pending.cancelled, &completion.cancelled)) { return; }
                                let mut published = false;
                                let mut publish = || {
                                    let Some(mut cache) = COLD_VIEWPORT_CACHE.try_lock() else { return; };
                                    if let Some(index) = cache.iter().position(|entry| entry.registration == registration.wire_identity()) {
                                        if let Some(entry) = cache.remove(index) { retirement.evicted.push(entry); }
                                    }
                                    while cache.len() >= 4 {
                                        if let Some(entry) = cache.pop_front() { retirement.evicted.push(entry); }
                                    }
                                    cache.push_back(ColdViewportEntry {
                                        registration: registration.wire_identity(), requested: publish_range.clone(), read: Arc::clone(&read),
                                    });
                                    published = true;
                                };
                                if let Some((sequence, dimensions)) = required_layout {
                                    let _ = pane.publish_line_reads_at_layout(std::slice::from_ref(read.as_ref()), sequence, dimensions, &mut publish);
                                } else {
                                    let _ = pane.publish_line_reads(std::slice::from_ref(read.as_ref()), &mut publish);
                                }
                                if published { *failure_state.lock() = None; }
                                #[cfg(test)]
                                if published { COLD_VIEWPORT_AFTER_PUBLISH.with(|hook| {
                                    let hook = hook.borrow_mut().take();
                                    if let Some(hook) = hook { hook(); }
                                }); }
                                // publish_line_reads has released terminal ownership.
                                // Capture only after the anchor's actual read was
                                // accepted, with the pending identity still held.
                                if published {
                                    if let Some(intent) = publish_intent.as_ref() {
                                        if let (Some(row), Ok(Some(layout))) = (read.resolve_viewport_anchor(&intent.anchor), pane.get_line_layout()) {
                                            let dimensions = layout.1;
                                            if same_line_layout_geometry(&dimensions, &intent.dimensions) {
                                                let first = row.max(dimensions.scrollback_top).min(dimensions.physical_top);
                                                if let Some(end) = StableRowIndex::try_from(dimensions.viewport_rows).ok().and_then(|rows| first.checked_add(rows)) {
                                                    let requested = publish_terminal.upgrade().and_then(|terminal| {
                                                        let term = terminal.try_lock()?;
                                                        term.screen().validates_line_read(&read).then(|| term.screen().expand_cold_logical_range(first..end))
                                                    });
                                                    if let Some(requested) = requested {
                                                    if read.cached_lines(requested.clone()).is_none() {
                                                        if let Some(Ok(plan)) = pane.capture_line_read(requested.clone(), &mut Default::default()) {
                                                            if let Some(pending) = pending.as_mut() { pending.requested = requested.clone(); }
                                                            retirement.followup = Some(ColdViewportFollowup { read: plan, requested, layout });
                                                        }
                                                    }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                drop(pending);
                                if published { pane.notify_lines_ready(); }
                                else { retry_cold_viewport(registration.clone(), Arc::clone(&retry)); }
                            });
                    }
                },
            );
            // Only the blocking worker waits. Queue cancellation drops the
            // retirement guard, so it also returns source ownership here.
            let Ok((read, evicted, followup)) = retired.recv() else { break; };
            // Retire prior payloads before hydrating another bounded plan.
            drop(read);
            drop(evicted);
            let Some(followup) = followup else { break; };
            if completion.cancelled.load(Ordering::Acquire) { break; }
            let ColdViewportFollowup { read, requested, layout } = followup;
            response_range = requested;
            required_layout = Some(layout);
            *worker_witness.lock() = Some(read.failure_witness());
            // The generic worker timer ends before this continuation. Keep
            // its hydration visible separately from publication wait time.
            let followup_started = log::log_enabled!(target: "mux::cold_read_profile", log::Level::Debug)
                .then(Instant::now);
            result = catch_recoverable(RecoverablePanicSite::MuxPaneCallback, AssertUnwindSafe(|| {
            if completion.cancelled.load(Ordering::Acquire) {
                Err(anyhow::anyhow!("cold read cancelled"))
            } else if read.requested_row_count() > frankenterm_term::screen::ScreenLineRead::MAX_ROWS {
                Err(anyhow::anyhow!("line read row limit"))
            } else { read.hydrate_with_payload_limit(
                frankenterm_term::screen::ScreenLineRead::MAX_PAYLOAD_BYTES,
                || completion.cancelled.load(Ordering::Acquire),
            ).map(|read| vec![read]) }
            })).unwrap_or_else(|_| Err(anyhow::anyhow!("cold read worker failed")));
            if let Some(started) = followup_started {
                log::debug!(target: "mux::cold_read_profile",
                    "cold_viewport_followup_complete hydrate_us={} read_ok={}",
                    started.elapsed().as_micros(), result.is_ok());
            }
            }
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
    fn drain_action_ring_into(action_ring: &ArrayQueue<AdmittedPaneActions>, term: &mut Terminal) {
        while let Some(actions) = action_ring.pop() {
            actions.apply(term);
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
    fn perform_actions_disruptor(&self, actions: AdmittedPaneActions) {
        if actions.actions.is_empty() {
            return;
        }
        if let Some(mut term) = self.terminal.try_lock() {
            self.drain_action_ring_locked(&mut term);
            actions.apply(&mut term);
            return;
        }
        if let Err(actions) = self.action_ring.push(actions) {
            let mut term = self.terminal.lock();
            self.drain_action_ring_locked(&mut term);
            actions.apply(&mut term);
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
        #[cfg(feature = "disruptor-pane-io")] action_ring: Arc<ArrayQueue<AdmittedPaneActions>>,
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
        #[cfg(feature = "disruptor-pane-io")] action_ring: Arc<ArrayQueue<AdmittedPaneActions>>,
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
                                term.capture_cold_seam_reflow().map(Some)
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
                                        let installed =
                                            term.install_cold_seam_reflow(&mut seam, seqno)?;
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
        #[cfg(feature = "disruptor-pane-io")] action_ring: &ArrayQueue<AdmittedPaneActions>,
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
        #[cfg(feature = "disruptor-pane-io")] action_ring: &ArrayQueue<AdmittedPaneActions>,
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
        spawn_custody: Option<crate::guardian_checkpoint::GuardianSpawnCaptureProvenanceV1>,
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
        if let Some(provenance) = ownership.spawn_custody {
            let identity = ownership.identity;
            anyhow::ensure!(
                provenance.original.guardian_incarnation == identity.guardian_incarnation
                    && provenance.original.pane_id == identity.pane_id
                    && provenance.current_mux_incarnation == identity.mux_incarnation
                    && provenance.current_lease_generation == identity.generation
                    && (identity.generation != 1
                        || provenance.original.mux_incarnation == identity.mux_incarnation),
                "guardian capture provenance differs from lease identity"
            );
            if let Some(successor) = provenance.acknowledged_successor {
                anyhow::ensure!(
                    successor.pane_id == identity.pane_id
                        && successor.successor.guardian_incarnation
                            == identity.guardian_incarnation
                        && successor.successor.mux_incarnation == identity.mux_incarnation
                        && successor.lease_generation == identity.generation,
                    "guardian successor provenance differs from lease identity"
                );
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
        let alert_staging = Arc::new(Mutex::new(PaneAlertStaging::default()));
        let child_exit_prune = ChildExitPruneState::new(Arc::clone(&mux_registration));
        let tmux_domain = Arc::new(Mutex::new(None));
        let (process, signaller, pid) = split_child(process, Arc::clone(&child_exit_prune));

        terminal.set_device_control_handler(Box::new(LocalPaneDCSHandler {
            pane_id,
            tmux_domain: Arc::clone(&tmux_domain),
            mux_registration: Arc::clone(&mux_registration),
        }));
        terminal.set_notification_handler(Box::new(LocalPaneNotifHandler {
            staging: Arc::clone(&alert_staging),
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
            cold_selection: Arc::new(Mutex::new(Vec::new())),
            cold_selection_retry: Arc::new(AtomicBool::new(false)),
            line_layout_observation: Arc::new(Mutex::new(None)),
            output_application: Mutex::new(()),
            alert_staging,
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
        for work in self.cold_selection.lock().drain(..) {
            work.cancelled.store(true, Ordering::Release);
        }
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

    #[test]
    fn alert_executor_initialization_failure_is_not_retryable_capacity() {
        use promise::spawn::BackgroundSpawnError;
        assert_eq!(
            background_alert_refusal(BackgroundSpawnError::WorkerUnavailable(
                "unavailable".into()
            )),
            PaneActionAdmissionRefusal::SchedulerUnavailable,
        );
        assert_eq!(
            background_alert_refusal(BackgroundSpawnError::TaskCapacityExhausted {
                active: 1,
                capacity: 1
            }),
            PaneActionAdmissionRefusal::Capacity,
        );
    }

    #[test]
    fn historical_alert_admission_preserves_fifo_bounds_and_shutdown() {
        const CHILD: &str = "FT_HISTORICAL_ALERT_CONTRACT_CHILD";
        if std::env::var_os(CHILD).as_deref() != Some(std::ffi::OsStr::new("1")) {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "localpane::tests::historical_alert_admission_preserves_fifo_bounds_and_shutdown", "--nocapture"])
                .env(CHILD, "1")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn().unwrap();
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                if child.try_wait().unwrap().is_some() {
                    let output = child.wait_with_output().unwrap();
                    assert!(
                        output.status.success(),
                        "alert subprocess failed: {}{}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    );
                    assert!(String::from_utf8_lossy(&output.stdout)
                        .contains("HISTORICAL_ALERT_CONTRACT_SUCCESS"));
                    return;
                }
                if Instant::now() >= deadline {
                    child.kill().unwrap();
                    let output = child.wait_with_output().unwrap();
                    panic!(
                        "alert subprocess timed out: {}{}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    );
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        fn pump_until(executor: &promise::spawn::SimpleExecutor, mut done: impl FnMut() -> bool) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !done() {
                while executor.try_tick().unwrap() {}
                assert!(
                    Instant::now() < deadline,
                    "alert publication did not complete"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        fn title(value: &str) -> Action {
            Action::OperatingSystemCommand(Box::new(
                termwiz::escape::osc::OperatingSystemCommand::SetWindowTitle(value.into()),
            ))
        }
        let domain: Arc<dyn Domain> =
            Arc::new(crate::domain::LocalDomain::new("alert-contract").unwrap());
        let pane = Arc::new(LocalPane::new(
            812,
            guardian_lifetime_test_terminal(),
            Box::new(ColdResizeTestChild::default()),
            Box::new(GuardianLifetimeTestMasterPty),
            Box::new(Vec::<u8>::new()),
            domain.domain_id(),
            [0xa1; 16],
            "alert-contract".into(),
        ));
        let mut model_only_actions = vec![title("before-registration")];
        let mut model_only_output = pane.prepare_alert_output().unwrap();
        assert!(model_only_output.is_none());
        let model_only = pane
            .admit_alert_actions(&mut model_only_actions, 0, &mut model_only_output)
            .unwrap();
        let dynamic: Arc<dyn Pane> = pane.clone();
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
                &dynamic,
                &generation,
            )
            .unwrap();
        }
        let registration = mux.capture_pane_registration(&dynamic).unwrap();
        pane.mux_registration
            .reserve(registration)
            .unwrap()
            .commit()
            .unwrap()
            .finalize();

        // A registered embedding with no scheduler refuses the original batch.
        let no_scheduler = pane
            .perform_actions(vec![title("not-applied")])
            .unwrap_err();
        assert_eq!(
            no_scheduler.reason,
            PaneActionAdmissionRefusal::SchedulerUnavailable
        );
        assert_eq!(no_scheduler.actions.len(), 1);
        let executor = promise::spawn::SimpleExecutor::try_with_limits(
            promise::spawn::MainThreadAdmissionLimits::new(32, 1024 * 1024, 0, 0).unwrap(),
        )
        .unwrap();
        // Exercise the production dispatch owner with a consumer that needs
        // another reader turn to finish. Publishing must return before that
        // turn; the second event must still wait for consumer completion.
        // This is the reader/completion dependency itself, not a native Lua test.
        let dispatch_queue = crate::PaneAlertDispatchQueue::default();
        let first_dispatch = crate::FundedPaneAlertDispatch::reserve(4096).unwrap();
        let second_dispatch = crate::FundedPaneAlertDispatch::reserve(4096).unwrap();
        let delivery_order = Arc::new(Mutex::new(Vec::new()));
        let first_started = Arc::new(AtomicBool::new(false));
        let mut reader_reply = promise::Promise::<()>::new();
        let consumer_reply = reader_reply.get_future().unwrap();
        let first_order = Arc::clone(&delivery_order);
        let started = Arc::clone(&first_started);
        first_dispatch.publish(&dispatch_queue, std::future::ready(()), async move {
            started.store(true, Ordering::Release);
            consumer_reply.await.unwrap();
            first_order.lock().push(1);
        });
        let second_order = Arc::clone(&delivery_order);
        second_dispatch.publish(&dispatch_queue, std::future::ready(()), async move {
            second_order.lock().push(2);
        });
        pump_until(&executor, || first_started.load(Ordering::Acquire));
        // A ready successor cannot pass the consumer still awaiting its reply.
        assert!(delivery_order.lock().is_empty());
        assert!(reader_reply.result(Ok(())));
        pump_until(&executor, || delivery_order.lock().len() == 2);
        assert_eq!(*delivery_order.lock(), [1, 2]);
        pump_until(&executor, || {
            executor.admission_snapshot().active_tasks == 0
        });
        // The first subscriber owns a real Render permit; refusal by the
        // second must roll it back before any terminal action is performed.
        struct SubscriberPermit {
            permit: Option<promise::spawn::MainThreadSpawnReservation>,
            released: Arc<AtomicUsize>,
        }
        impl Drop for SubscriberPermit {
            fn drop(&mut self) {
                drop(self.permit.take());
                self.released.fetch_add(1, Ordering::Release);
            }
        }
        let subscriber_admissions = Arc::new(AtomicUsize::new(0));
        let subscriber_releases = Arc::new(AtomicUsize::new(0));
        let admitted = Arc::clone(&subscriber_admissions);
        let released = Arc::clone(&subscriber_releases);
        let first_subscriber = mux
            .subscribe_historical_alerts(
                |_| true,
                |_| Some((1, 4096)),
                move |_| {
                    let permit = match promise::spawn::try_reserve_main_thread(
                        promise::spawn::MainThreadServiceClass::Render,
                        4096,
                    ) {
                        promise::spawn::MainThreadReservationOutcome::Reserved(permit) => permit,
                        _ => return Err(PaneActionAdmissionRefusal::Capacity),
                    };
                    admitted.fetch_add(1, Ordering::Release);
                    let permit = SubscriberPermit {
                        permit: Some(permit),
                        released: Arc::clone(&released),
                    };
                    Ok(Box::new(move |_, _, completion| {
                        drop(permit);
                        drop(completion);
                    }))
                },
            )
            .unwrap();
        let refusing_subscriber = mux
            .subscribe_historical_alerts(
                |_| true,
                |_| Some((1, 4096)),
                |_| Err(PaneActionAdmissionRefusal::Capacity),
            )
            .unwrap();
        let before_title = pane.get_title();
        let refused = pane
            .perform_actions(vec![
                title("must-not-commit"),
                Action::Control(termwiz::escape::ControlCode::Bell),
            ])
            .unwrap_err();
        assert_eq!(refused.reason, PaneActionAdmissionRefusal::Capacity);
        assert_eq!(refused.actions.len(), 2);
        assert_eq!(pane.get_title(), before_title);
        assert_eq!(subscriber_admissions.load(Ordering::Acquire), 1);
        assert_eq!(subscriber_releases.load(Ordering::Acquire), 1);
        // Output authority reserves its ordinary notification drain before
        // historical admission. That queued task is not a leaked subscriber
        // permit: the witness above proves rollback before any executor turn.
        assert!(mux.pane_output_drain_scheduled.load(Ordering::Acquire));
        assert_eq!(executor.admission_snapshot().active_tasks, 1);
        pump_until(&executor, || {
            executor.admission_snapshot().active_tasks == 0
        });
        assert_eq!(executor.admission_snapshot().active_tasks, 0);
        assert!(!mux.pane_output_drain_scheduled.load(Ordering::Acquire));
        drop(refusing_subscriber);
        // All credits are known to belong to this batch, without consulting
        // occupancy: 32 Bell consumers plus its dispatcher cannot fit 32 slots.
        let oversized = pane
            .perform_actions(vec![
                Action::Control(termwiz::escape::ControlCode::Bell);
                32
            ])
            .unwrap_err();
        assert_eq!(oversized.reason, PaneActionAdmissionRefusal::SizeOverflow);
        assert_eq!(oversized.actions.len(), 32);
        assert_eq!(subscriber_admissions.load(Ordering::Acquire), 1);
        assert_eq!(subscriber_releases.load(Ordering::Acquire), 1);
        assert!(mux.pane_output_drain_scheduled.load(Ordering::Acquire));
        assert_eq!(executor.admission_snapshot().active_tasks, 1);
        pump_until(&executor, || {
            executor.admission_snapshot().active_tasks == 0
        });
        assert_eq!(executor.admission_snapshot().active_tasks, 0);
        assert!(!mux.pane_output_drain_scheduled.load(Ordering::Acquire));
        drop(first_subscriber);
        let tab = Arc::new(crate::tab::Tab::new(&term_size(80, 24)));
        mux.add_tab_no_panes(&tab).unwrap();
        let window = mux.new_empty_window(None, None);
        let window_id = *window;
        mux.add_tab_to_window(&tab, window_id).unwrap();
        drop(window);
        tab.assign_pane(&dynamic);
        // Actual parser batch, two historical events: a pending first
        // consumer must prevent the second callback from even starting.
        let held = Arc::new(Mutex::new(None::<crate::HistoricalAlertCompletion>));
        let historical_names = Arc::new(Mutex::new(Vec::new()));
        let held_for_callback = Arc::clone(&held);
        let names_for_callback = Arc::clone(&historical_names);
        let history = mux
            .subscribe_historical_alerts(
                move |window| window == Some(window_id),
                |_| Some((0, 0)),
                move |_| {
                    let held = Arc::clone(&held_for_callback);
                    let names = Arc::clone(&names_for_callback);
                    Ok(Box::new(move |_, alert, completion| {
                        let Alert::SetUserVar { name, .. } = alert else {
                            panic!("expected user variable")
                        };
                        names.lock().push(name);
                        assert!(held.lock().replace(completion).is_none());
                    }))
                },
            )
            .unwrap();
        // An unrelated window must not charge or invoke its admission at all.
        let unrelated = mux
            .subscribe_historical_alerts(
                move |window| window != Some(window_id),
                |_| Some((usize::MAX, usize::MAX)),
                |_| panic!("unrelated historical subscriber was admitted"),
            )
            .unwrap();
        let user_var = |name: &str| {
            Action::OperatingSystemCommand(Box::new(
                termwiz::escape::osc::OperatingSystemCommand::ITermProprietary(
                    termwiz::escape::osc::ITermProprietary::SetUserVar {
                        name: name.into(),
                        value: "value".into(),
                    },
                ),
            ))
        };
        pane.perform_actions(vec![user_var("first"), user_var("second")])
            .unwrap();
        pump_until(&executor, || historical_names.lock().len() == 1);
        while executor.try_tick().unwrap() {}
        assert_eq!(&*historical_names.lock(), &["first"]);
        drop(held.lock().take());
        pump_until(&executor, || historical_names.lock().len() == 2);
        assert_eq!(&*historical_names.lock(), &["first", "second"]);
        drop(held.lock().take());
        pump_until(&executor, || {
            executor.admission_snapshot().active_tasks == 0
        });
        // Retirement cancels an admitted, not-yet-called successor receipt.
        // Neither that receipt nor its FIFO successor can wait forever.
        pane.perform_actions(vec![user_var("third"), user_var("cancelled")])
            .unwrap();
        pump_until(&executor, || historical_names.lock().len() == 3);
        drop(history);
        drop(held.lock().take());
        pump_until(&executor, || {
            executor.admission_snapshot().active_tasks == 0
        });
        assert_eq!(&*historical_names.lock(), &["first", "second", "third"]);
        drop(unrelated);
        let received = Arc::new(Mutex::new(Vec::<Alert>::new()));
        let observed = received.clone();
        let weak_pane = Arc::downgrade(&pane);
        let wrong_thread = Arc::new(AtomicBool::new(false));
        let lock_violation = Arc::new(AtomicBool::new(false));
        let callback_thread = wrong_thread.clone();
        let callback_lock = lock_violation.clone();
        let callback_tab = Arc::downgrade(&tab);
        let owner_thread = std::thread::current().id();
        mux.subscribe(move |notification| {
            if let crate::MuxNotification::Alert { alert, .. } = notification {
                callback_thread.fetch_or(
                    std::thread::current().id() != owner_thread,
                    Ordering::SeqCst,
                );
                let pane = weak_pane.upgrade().unwrap();
                callback_lock.fetch_or(pane.terminal.try_lock().is_none(), Ordering::SeqCst);
                callback_lock.fetch_or(
                    !callback_tab
                        .upgrade()
                        .unwrap()
                        .topology_lock_is_available_for_test(),
                    Ordering::SeqCst,
                );
                observed.lock().push(alert);
            }
            true
        })
        .unwrap();

        // An unregistered batch cannot acquire notification authority midway
        // through application merely because the pane was registered meanwhile.
        model_only.apply(&mut pane.terminal.lock());
        assert!(received.lock().is_empty());
        let handoffs = ALERT_MAIN_HANDOFFS.load(Ordering::Acquire);
        pane.perform_actions(vec![title("A")]).unwrap();
        pane.perform_actions(vec![Action::Print('x')]).unwrap();
        pane.perform_actions(vec![title("B")]).unwrap();
        assert!(received.lock().is_empty());
        pump_until(&executor, || {
            received
                .lock()
                .iter()
                .filter(|alert| matches!(alert, Alert::WindowTitleChanged(_)))
                .count()
                == 2
        });
        let titles = received
            .lock()
            .iter()
            .filter_map(|alert| match alert {
                Alert::WindowTitleChanged(s) => Some(s.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(titles, ["A", "B"]);
        assert_eq!(mux.get_window(window_id).unwrap().get_title(), "B");
        pump_until(&executor, || {
            executor.admission_snapshot().active_tasks == 0
        });
        assert_eq!(
            ALERT_MAIN_HANDOFFS.load(Ordering::Acquire),
            handoffs + 2,
            "the intervening no-alert output must not enqueue a main-thread callback"
        );

        // Exercise the production admission/application helper while retaining
        // the emitting guard. Even pumping the main executor cannot dispatch it.
        let before = received.lock().len();
        let mut output = pane.prepare_alert_output().unwrap();
        let mut actions = vec![Action::Control(termwiz::escape::ControlCode::Bell)];
        let batch = pane
            .admit_alert_actions(&mut actions, 0, &mut output)
            .unwrap();
        let mut emitting = pane.terminal.lock();
        batch.apply(&mut emitting);
        for _ in 0..20 {
            while executor.try_tick().unwrap() {}
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(received.lock().len(), before);
        drop(emitting);
        pump_until(&executor, || received.lock().len() > before);
        pump_until(&executor, || {
            executor.admission_snapshot().active_tasks == 0
        });

        // Reconciliation holds the real tab topology mutex while waiting for
        // this pane's terminal dimensions. Applying a title must not invoke
        // subscribers inline in that state. The indexed window-title lookup
        // itself does not take the tab mutex; this proves callback isolation
        // during concurrent reconciliation, not a window-title ABBA cycle.
        let before = received.lock().len();
        let mut output = pane.prepare_alert_output().unwrap();
        let mut actions = vec![title("concurrent-reconciliation")];
        let batch = pane
            .admit_alert_actions(&mut actions, 0, &mut output)
            .unwrap();
        let mut emitting = pane.terminal.lock();
        let worker_tab = tab.clone();
        let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
        let reconcile = std::thread::spawn(move || {
            worker_tab.rebuild_splits_sizes_from_contained_panes();
            done_tx.send(()).unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while tab.topology_lock_is_available_for_test() {
            assert!(
                Instant::now() < deadline,
                "reconciliation did not acquire topology"
            );
            std::thread::yield_now();
        }
        batch.apply(&mut emitting);
        assert_eq!(received.lock().len(), before);
        drop(emitting);
        done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        reconcile.join().unwrap();
        pump_until(&executor, || received.lock().len() > before);
        assert_eq!(
            mux.get_window(window_id).unwrap().get_title(),
            "concurrent-reconciliation"
        );
        assert!(!lock_violation.load(Ordering::SeqCst));
        pump_until(&executor, || {
            executor.admission_snapshot().active_tasks == 0
        });

        // A later ST owns no text itself but emits two retained Unicode titles.
        let unicode = "界🙂".repeat(512);
        pane.perform_actions(vec![
            Action::Esc(termwiz::escape::Esc::Code(
                termwiz::escape::EscCode::TmuxTitle,
            )),
            Action::PrintString(unicode.clone()),
        ])
        .unwrap();
        pane.perform_actions(vec![Action::Esc(termwiz::escape::Esc::Code(
            termwiz::escape::EscCode::StringTerminator,
        ))])
        .unwrap();
        pump_until(&executor, || {
            received
                .lock()
                .iter()
                .any(|alert| matches!(alert, Alert::WindowTitleChanged(s) if s == &unicode))
        });
        assert!(received
            .lock()
            .iter()
            .any(|alert| matches!(alert, Alert::IconTitleChanged(Some(s)) if s == &unicode)));
        pump_until(&executor, || {
            executor.admission_snapshot().active_tasks == 0
        });

        let mut saturation = Vec::new();
        loop {
            match promise::spawn::try_reserve_main_thread(
                promise::spawn::MainThreadServiceClass::Interactive,
                1,
            ) {
                promise::spawn::MainThreadReservationOutcome::Reserved(permit) => {
                    saturation.push(permit)
                }
                promise::spawn::MainThreadReservationOutcome::RetryableFull(_) => break,
                other => panic!("unexpected saturation result: {:?}", other),
            }
        }
        let seqno = pane.terminal.lock().current_seqno();
        let secret = "secret-canary-never-format";
        let rejected = pane.perform_actions(vec![title(secret)]).unwrap_err();
        assert_eq!(rejected.reason, PaneActionAdmissionRefusal::Capacity);
        assert_eq!(pane.terminal.lock().current_seqno(), seqno);
        assert!(!format!("{rejected:?} {rejected}").contains(secret));
        assert!(
            matches!(&rejected.actions[..], [Action::OperatingSystemCommand(command)] if matches!(command.as_ref(), termwiz::escape::osc::OperatingSystemCommand::SetWindowTitle(s) if s == secret))
        );
        drop(saturation);
        pane.perform_actions(rejected.actions).unwrap();
        pump_until(&executor, || {
            received
                .lock()
                .iter()
                .any(|alert| matches!(alert, Alert::WindowTitleChanged(s) if s == secret))
        });
        pump_until(&executor, || {
            executor.admission_snapshot().active_tasks == 0
        });
        assert!(!wrong_thread.load(Ordering::SeqCst));
        assert!(!lock_violation.load(Ordering::SeqCst));

        // Independent byte-pressure control: task slots are empty, but the
        // owned title alone fills the configured byte limit before overhead.
        // The old flat 4 KiB estimate incorrectly accepted this same payload.
        let seqno = pane.terminal.lock().current_seqno();
        let oversized = pane
            .perform_actions(vec![title(&"x".repeat(1024 * 1024))])
            .unwrap_err();
        assert_eq!(oversized.reason, PaneActionAdmissionRefusal::SizeOverflow);
        assert_eq!(oversized.actions.len(), 1);
        assert_eq!(pane.terminal.lock().current_seqno(), seqno);
        drop(oversized);
        pump_until(&executor, || {
            executor.admission_snapshot().active_tasks == 0
        });

        // Registration-only fixture: the helper enforces the real retirement
        // and in-flight generation fences without starting a fake PTY reader.
        // This proves alert custody/reuse, not reader startup or recovery.
        let make_retirement_pane = || {
            Arc::new(LocalPane::new(
                813,
                guardian_lifetime_test_terminal(),
                Box::new(ColdResizeTestChild::default()),
                Box::new(GuardianLifetimeTestMasterPty),
                Box::new(Vec::<u8>::new()),
                dynamic.domain_id(),
                [0xa2; 16],
                "alert-retirement".into(),
            ))
        };
        let retiring = make_retirement_pane();
        let retiring_dynamic: Arc<dyn Pane> = retiring.clone();
        let retiring_generation = crate::PaneRegistrationGeneration::new(
            813,
            &mux.pane_retirements,
            Arc::downgrade(&mux),
        );
        {
            let _registration = mux.pane_registration.lock();
            mux.insert_pane_registration_locked(
                813,
                dynamic.domain_id(),
                &retiring_dynamic,
                &retiring_generation,
            )
            .unwrap();
        }
        retiring
            .mux_registration
            .reserve(mux.capture_pane_registration(&retiring_dynamic).unwrap())
            .unwrap()
            .commit()
            .unwrap()
            .finalize();
        let retirement_events = Arc::new(Mutex::new(Vec::new()));
        let events = retirement_events.clone();
        mux.subscribe(move |notification| {
            match notification {
                crate::MuxNotification::Alert {
                    pane_id: 813,
                    alert: Alert::WindowTitleChanged(title),
                } => {
                    events.lock().push(title.clone());
                }
                crate::MuxNotification::PaneRemoved(813) => {
                    events.lock().push("removed".into());
                }
                _ => {}
            }
            true
        })
        .unwrap();
        retiring
            .perform_actions(vec![title("accepted-before-retirement")])
            .unwrap();
        mux.remove_pane_if_same(813, &retiring_dynamic);
        let replacement = make_retirement_pane();
        let replacement_dynamic: Arc<dyn Pane> = replacement.clone();
        let replacement_generation = crate::PaneRegistrationGeneration::new(
            813,
            &mux.pane_retirements,
            Arc::downgrade(&mux),
        );
        {
            let _registration = mux.pane_registration.lock();
            assert!(mux
                .insert_pane_registration_locked(
                    813,
                    dynamic.domain_id(),
                    &replacement_dynamic,
                    &replacement_generation,
                )
                .is_err());
        }
        pump_until(&executor, || retirement_events.lock().len() == 2);
        assert_eq!(
            *retirement_events.lock(),
            ["accepted-before-retirement", "removed"]
        );
        {
            let _registration = mux.pane_registration.lock();
            mux.insert_pane_registration_locked(
                813,
                dynamic.domain_id(),
                &replacement_dynamic,
                &replacement_generation,
            )
            .unwrap();
        }
        replacement
            .mux_registration
            .reserve(mux.capture_pane_registration(&replacement_dynamic).unwrap())
            .unwrap()
            .commit()
            .unwrap()
            .finalize();
        let old_seqno = retiring.terminal.lock().current_seqno();
        let successor_seqno = replacement.terminal.lock().current_seqno();
        let stale = retiring
            .perform_actions(vec![title("stale-title")])
            .unwrap_err();
        assert_eq!(stale.reason, PaneActionAdmissionRefusal::Retired);
        assert_eq!(stale.actions, vec![title("stale-title")]);
        assert_eq!(retiring.terminal.lock().current_seqno(), old_seqno);
        assert_eq!(replacement.terminal.lock().current_seqno(), successor_seqno);
        replacement
            .perform_actions(vec![title("successor-title")])
            .unwrap();
        pump_until(&executor, || retirement_events.lock().len() == 3);
        assert_eq!(
            *retirement_events.lock(),
            ["accepted-before-retirement", "removed", "successor-title"]
        );
        pump_until(&executor, || {
            executor.admission_snapshot().active_tasks == 0
        });

        // Close the exact scheduler while two accepted batches are fenced by
        // the emitting guard. They cancel, release FIFO and output custody, and
        // do not run subscriber callbacks on the producer or background thread.
        let cancellations = ALERT_DELIVERY_CANCELLED.load(Ordering::Acquire);
        let before = received.lock().len();
        let mut first_output = pane.prepare_alert_output().unwrap();
        let mut second_output = pane.prepare_alert_output().unwrap();
        let mut first = vec![title("cancelled-first")];
        let mut second = vec![title("cancelled-second")];
        let first = pane
            .admit_alert_actions(&mut first, 0, &mut first_output)
            .unwrap();
        let second = pane
            .admit_alert_actions(&mut second, 0, &mut second_output)
            .unwrap();
        let mut emitting = pane.terminal.lock();
        first.apply(&mut emitting);
        second.apply(&mut emitting);
        drop(executor);
        assert_eq!(received.lock().len(), before);
        drop(emitting);
        let deadline = Instant::now() + Duration::from_secs(5);
        while ALERT_DELIVERY_CANCELLED.load(Ordering::Acquire) < cancellations + 2
            || generation.operation_state.load(Ordering::Acquire)
                & crate::PANE_REGISTRATION_OPERATION_MASK
                != 0
        {
            assert!(
                Instant::now() < deadline,
                "closed scheduler stranded funded alert custody"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(received.lock().len(), before);
        assert_eq!(promise::spawn::main_thread_admission_accounting_errors(), 0);
        let dead = Arc::new(AtomicBool::new(false));
        crate::send_actions_to_mux_with_scheduler_state(
            &Arc::downgrade(&dynamic),
            &generation,
            &dead,
            vec![title("refused-parser-boundary")],
            true,
        );
        assert!(dead.load(Ordering::Acquire));
        assert!(generation
            .live_parser_checkpoint
            .state
            .lock()
            .poison
            .is_some());
        assert!(
            generation
                .live_parser_checkpoint
                .record_parsed_bytes(0)
                .is_err(),
            "a refused batch must never become an acknowledged checkpoint boundary"
        );
        println!("HISTORICAL_ALERT_CONTRACT_SUCCESS");
    }

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
        refuse_next_payload: AtomicBool,
        payload_refusals: AtomicUsize,
        read_gate: Mutex<Option<(std::sync::mpsc::SyncSender<()>, Receiver<()>)>>,
        read_gate_row: Mutex<Option<StableRowIndex>>,
        panic_next_payload: AtomicBool,
        payload_panics: AtomicUsize,
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
            if self.refuse_next_payload.swap(false, Ordering::AcqRel) {
                self.payload_refusals.fetch_add(1, Ordering::Release);
                return None;
            }
            let gate = if self
                .read_gate_row
                .lock()
                .is_none_or(|expected| expected == row)
            {
                self.read_gate.lock().take()
            } else {
                None
            };
            if let Some((entered, release)) = gate {
                entered.send(()).unwrap();
                release.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            if self.panic_next_payload.swap(false, Ordering::AcqRel) {
                self.payload_panics.fetch_add(1, Ordering::Release);
                panic!("one-shot cold viewport sink failure");
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

    fn isolated_search_test(name: &str) -> bool {
        const CHILD: &str = "FT_ISOLATED_SEARCH_TEST";
        if std::env::var_os(CHILD).as_deref() == Some(std::ffi::OsStr::new(name)) {
            return false;
        }
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env(CHILD, name)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if child.try_wait().unwrap().is_some() {
                let output = child.wait_with_output().unwrap();
                assert!(
                    output.status.success(),
                    "{}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
                return true;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                let output = child.wait_with_output().unwrap();
                panic!(
                    "search subprocess timed out: {}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn cold_search_finds_retained_paragraph_and_resident_control() {
        if isolated_search_test(
            "localpane::tests::cold_search_finds_retained_paragraph_and_resident_control",
        ) {
            return;
        }
        let (pane, _mux, _registration, _old_sink, _token) = cold_resize_fixture(false);
        let sink = Arc::new(ColdResizeTestSink::default());
        let mut term = Terminal::new(
            term_size(80, 4),
            Arc::new(ColdResizeTestConfig(Arc::clone(&sink))),
            "FrankenTerm",
            "cold-search",
            Box::new(Vec::new()),
        );
        let paragraph = format!(
            "RAGGED_04806 {}{} END_04806",
            "ab 界 e\u{301} 🚀 xy\u{a0}z ".repeat(7),
            "q".repeat(64)
        );
        assert_eq!(paragraph.len(), 241);
        term.advance_bytes(paragraph.as_bytes());
        term.advance_bytes(b"\r\n");
        for _ in 0..12 {
            term.advance_bytes(b"later unselected history\r\n");
        }
        term.advance_bytes(b"RESIDENT_SEARCH_CONTROL");
        *pane.terminal.lock() = term;
        let (range, resident_start) = {
            let term = pane.terminal.lock();
            let (first, count) = term.screen().scrollback_geometry();
            (
                first..first + count as StableRowIndex,
                term.screen().phys_to_stable_row_index(0),
            )
        };
        let read = pane
            .capture_line_read(range.clone(), &mut Default::default())
            .unwrap()
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        assert!(pane.terminal.lock().screen().validates_line_read(&read));
        let selected_row = read.first_row()
            + read
                .lines()
                .position(|line| line.as_str().starts_with("RAGGED_04806"))
                .expect("real cold hydration must contain the searched paragraph")
                as StableRowIndex;
        assert!(selected_row < resident_start, "target must be cold-only");
        drop(read);
        let resident = promise::spawn::block_on(pane.search(
            Pattern::CaseSensitiveString("RESIDENT_SEARCH_CONTROL".into()),
            range.clone(),
            Some(10),
        ))
        .unwrap();
        assert_eq!(resident.len(), 1, "resident search positive control");
        let cold = promise::spawn::block_on(pane.search(
            Pattern::CaseSensitiveString("RAGGED_04806".into()),
            range.clone(),
            Some(10),
        ))
        .unwrap();
        assert_eq!(cold.len(), 1, "search must include retained cold payload");
        assert_eq!(cold[0].start_y, selected_row);
        assert_eq!(cold[0].start_x, 0);
        assert_eq!(cold[0].end_x, "RAGGED_04806".len());
        for cols in [69, 106] {
            pane.terminal.lock().resize(term_size(cols, 4));
            // No preparation call: search itself must publish newly hydrated
            // geometry before interpreting rebased stable coordinates.
            let matches = promise::spawn::block_on(pane.search(
                Pattern::CaseSensitiveString(paragraph.clone()),
                StableRowIndex::MIN..StableRowIndex::MAX,
                Some(10),
            ))
            .unwrap();
            assert_eq!(matches.len(), 1, "whole paragraph after resize to {}", cols);
            let found = matches[0];
            let read = pane
                .capture_line_read(found.start_y..found.end_y + 1, &mut Default::default())
                .unwrap()
                .unwrap()
                .hydrate(|| false)
                .unwrap();
            assert!(pane.terminal.lock().screen().validates_line_read(&read));
            let mut actual = String::new();
            for (offset, line) in read.lines().enumerate() {
                let row = read.first_row() + offset as StableRowIndex;
                for cell in line.visible_cells() {
                    if (row != found.start_y || cell.cell_index() >= found.start_x)
                        && (row != found.end_y || cell.cell_index() < found.end_x)
                    {
                        actual.push_str(cell.str());
                    }
                }
            }
            assert_eq!(actual, paragraph);
        }
        let range = StableRowIndex::MIN..StableRowIndex::MAX;
        // Reserve the other three slots in this isolated process. Recovery
        // below can only run after the abandoned worker releases its own slot.
        let mut held = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while held.len() < 4 {
            if let Some(permit) = crate::pane::LineReadPermit::try_acquire() {
                held.push(permit);
            } else {
                assert!(Instant::now() < deadline, "search worker did not retire");
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        drop(held.pop());
        // Pause a real payload read. Search must not own the terminal mutex
        // while blocked on storage, and abandoning its future must be safe.
        use std::future::Future;
        let (entered_tx, entered_rx) = sync_channel(1);
        let (release_tx, release_rx) = sync_channel(1);
        *sink.read_gate.lock() = Some((entered_tx, release_rx));
        let mut abandoned = Box::pin(pane.search(
            Pattern::CaseSensitiveString("RAGGED_04806".into()),
            range.clone(),
            Some(10),
        ));
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        let pending = abandoned.as_mut().poll(&mut context).is_pending();
        let entered = entered_rx.recv_timeout(Duration::from_secs(3));
        let terminal_free = pane.terminal.try_lock().is_some();
        drop(abandoned);
        let _ = release_tx.send(());
        sink.read_gate.lock().take();
        assert!(pending && entered.is_ok() && terminal_free);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let recovered = promise::spawn::block_on(pane.search(
                Pattern::CaseSensitiveString("RAGGED_04806".into()),
                range.clone(),
                Some(10),
            ));
            if let Ok(matches) = recovered {
                assert_eq!(matches.len(), 1);
                break;
            }
            assert!(
                Instant::now() < deadline,
                "cancelled search must release its worker"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        drop(held);
        // Prune real retained source after capture but before hydration.
        let (entered_tx, entered_rx) = sync_channel(1);
        let (release_tx, release_rx) = sync_channel(1);
        *sink.read_gate.lock() = Some((entered_tx, release_rx));
        let mut stale = Box::pin(pane.search(
            Pattern::CaseSensitiveString("RAGGED_04806".into()),
            range,
            Some(10),
        ));
        let pending = stale.as_mut().poll(&mut context).is_pending();
        let entered = entered_rx.recv_timeout(Duration::from_secs(3));
        let removed = sink.rows.lock().1.remove(&selected_row).is_some();
        let _ = release_tx.send(());
        sink.read_gate.lock().take();
        assert!(pending && entered.is_ok() && removed);
        assert!(
            promise::spawn::block_on(stale).is_err(),
            "pruned search must not publish empty success"
        );
    }

    #[test]
    fn search_preserves_wrap_and_internal_chunk_boundary_coordinates() {
        if isolated_search_test(
            "localpane::tests::search_preserves_wrap_and_internal_chunk_boundary_coordinates",
        ) {
            return;
        }
        #[derive(Debug)]
        struct BoundaryConfig(ColdResizeTestConfig, usize);
        impl TerminalConfiguration for BoundaryConfig {
            fn color_palette(&self) -> ColorPalette {
                self.0.color_palette()
            }
            fn scrollback_size(&self) -> usize {
                // The shared resize fixture retains only 32 rows. This corpus
                // needs all 257 paragraph rows plus its closing control lines.
                self.1
            }
            fn scrollback_tier_config(&self) -> frankenterm_term::config::ScrollbackTierConfig {
                self.0.scrollback_tier_config()
            }
            fn scrollback_spill_sink(
                &self,
            ) -> Option<Arc<dyn frankenterm_term::config::ScrollbackSpillSink>> {
                self.0.scrollback_spill_sink()
            }
        }
        let (pane, _mux, _registration, _old_sink, _token) = cold_resize_fixture(false);
        let sink = Arc::new(ColdResizeTestSink::default());
        let mut term = Terminal::new(
            term_size(4, 1010),
            Arc::new(ColdResizeTestConfig(sink)),
            "FrankenTerm",
            "search-boundary",
            Box::new(Vec::new()),
        );
        for _ in 0..999 {
            term.advance_bytes(b"x\r\n");
        }
        term.advance_bytes(b"aaNEEDLEzz");
        *pane.terminal.lock() = term;
        let matches = promise::spawn::block_on(pane.search(
            Pattern::CaseSensitiveString("NEEDLE".into()),
            0..1010,
            Some(10),
        ))
        .unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!((matches[0].start_y, matches[0].start_x), (999, 2));
        assert_eq!((matches[0].end_y, matches[0].end_x), (1001, 0));
        for rows in [300, 2] {
            for (prefix, tail, literal, start_x) in [
                (1023, "XY", "XY", 3),
                (1022, "界Y", "界Y", 2),
                (1023, "İY", "İY", 3),
            ] {
                let sink = Arc::new(ColdResizeTestSink::default());
                let mut term = Terminal::new(
                    term_size(4, rows),
                    Arc::new(BoundaryConfig(ColdResizeTestConfig(Arc::clone(&sink)), 512)),
                    "FrankenTerm",
                    "search-logical-boundary",
                    Box::new(Vec::new()),
                );
                term.advance_bytes(format!("{}{}", "a".repeat(prefix), tail).as_bytes());
                if rows == 2 {
                    // This fallback sink deliberately supplies no spill-admission
                    // witness. Close the paragraph before moving BOTH endpoints
                    // into cold storage; an open unwitnessed cold/resident seam
                    // cannot authorize a coherent layout for this fixture.
                    term.advance_bytes(b"\r\nnext\r\nnext\r\nnext\r\n");
                    assert!(term.screen().phys_to_stable_row_index(0) > 256);
                    let retained = sink.rows.lock();
                    assert!(retained.1.len() > 256 && retained.1.len() <= 512);
                    assert!(retained.1.contains_key(&255));
                    assert!(retained.1.contains_key(&256));
                }
                *pane.terminal.lock() = term;
                for pattern in [
                    Pattern::CaseSensitiveString(literal.into()),
                    Pattern::CaseInSensitiveString(literal.to_lowercase()),
                    Pattern::Regex(literal.into()),
                ] {
                    let found = promise::spawn::block_on(pane.search(
                        pattern,
                        StableRowIndex::MIN..StableRowIndex::MAX,
                        Some(10),
                    ))
                    .unwrap();
                    assert_eq!(found.len(), 1);
                    assert_eq!((found[0].start_y, found[0].start_x), (255, start_x));
                    assert_eq!((found[0].end_y, found[0].end_x), (256, 1));
                }
                if rows == 2 {
                    assert!(sink.payload_reads.load(Ordering::Acquire) > 0);
                }
            }
        }
        let mut term = Terminal::new(
            term_size(4, 1100),
            Arc::new(ColdResizeTestConfig(
                Arc::new(ColdResizeTestSink::default()),
            )),
            "FrankenTerm",
            "search-overlapping-chunk-context",
            Box::new(Vec::new()),
        );
        term.advance_bytes("a".repeat(4003).as_bytes());
        *pane.terminal.lock() = term;
        let found = promise::spawn::block_on(pane.search(
            Pattern::CaseSensitiveString("aaa".into()),
            0..1100,
            None,
        ))
        .unwrap();
        assert_eq!(found.len(), 4003 / 3);
        for (index, found) in found.iter().enumerate() {
            let start = index * 3;
            let end = start + 3;
            assert_eq!(
                (found.start_y, found.start_x),
                ((start / 4) as StableRowIndex, start % 4)
            );
            assert_eq!(
                (found.end_y, found.end_x),
                ((end / 4) as StableRowIndex, end % 4)
            );
        }
        let subrange = promise::spawn::block_on(pane.search(
            Pattern::CaseSensitiveString("aaa".into()),
            1000..1100,
            None,
        ))
        .unwrap();
        assert_eq!(
            subrange.len(),
            0,
            "the last global non-overlapping match starts before row1000"
        );
        let anchored =
            promise::spawn::block_on(pane.search(Pattern::Regex("^aaa".into()), 0..1100, None))
                .unwrap();
        assert_eq!(
            anchored.len(),
            1,
            "chunk overlap must not create a regex line start"
        );
        assert_eq!((anchored[0].start_y, anchored[0].start_x), (0, 0));
        for (prefix, tail, query, start_column) in
            [(3999, "İY", "i\u{307}y", 3), (3998, "界Y", "界y", 2)]
        {
            let mut term = Terminal::new(
                term_size(4, 1100),
                Arc::new(ColdResizeTestConfig(
                    Arc::new(ColdResizeTestSink::default()),
                )),
                "FrankenTerm",
                "streaming-unicode-boundary",
                Box::new(Vec::new()),
            );
            term.advance_bytes(format!("{}{}", "a".repeat(prefix), tail).as_bytes());
            *pane.terminal.lock() = term;
            let found = promise::spawn::block_on(pane.search(
                Pattern::CaseInSensitiveString(query.into()),
                0..1100,
                Some(10),
            ))
            .unwrap();
            assert_eq!(found.len(), 1);
            assert_eq!((found[0].start_y, found[0].start_x), (999, start_column));
            assert_eq!((found[0].end_y, found[0].end_x), (1000, 1));
            let suffix = promise::spawn::block_on(pane.search(
                Pattern::CaseInSensitiveString(query.into()),
                999..1000,
                Some(10),
            ))
            .unwrap();
            assert_eq!(
                suffix, found,
                "match starting in range must finish beyond its end"
            );
        }
        let mut term = Terminal::new(
            term_size(4, 1100),
            Arc::new(ColdResizeTestConfig(
                Arc::new(ColdResizeTestSink::default()),
            )),
            "FrankenTerm",
            "streaming-sigma",
            Box::new(Vec::new()),
        );
        term.advance_bytes(format!("{}ΟΣ οσ ος Σ", " ".repeat(3999)).as_bytes());
        *pane.terminal.lock() = term;
        // Greek sigma forms denote the same letter in a case-insensitive
        // literal. Scalar lowercase plus explicit sigma equivalence makes
        // this independent of a physical capture ending between Ο and Σ.
        for query in ["ΟΣ", "οσ", "ος"] {
            let found = promise::spawn::block_on(pane.search(
                Pattern::CaseInSensitiveString(query.into()),
                0..1100,
                Some(10),
            ))
            .unwrap();
            assert_eq!(found.len(), 3);
            for (result, column) in found.iter().zip([3999usize, 4002, 4005]) {
                assert_eq!(
                    (result.start_y, result.start_x),
                    ((column / 4) as StableRowIndex, column % 4)
                );
                assert_eq!(
                    (result.end_y, result.end_x),
                    (((column + 2) / 4) as StableRowIndex, (column + 2) % 4)
                );
            }
        }
        // Literals stream through a group larger than the captured context.
        // Regex must still refuse incomplete context rather than reinterpret
        // anchors or claim a complete empty result.
        let mut term = Terminal::new(
            term_size(4, 4100),
            Arc::new(ColdResizeTestConfig(
                Arc::new(ColdResizeTestSink::default()),
            )),
            "FrankenTerm",
            "search-incomplete-group",
            Box::new(Vec::new()),
        );
        term.advance_bytes(b"FOUND\r\n");
        term.advance_bytes(format!("{}XY", "a".repeat(14_000)).as_bytes());
        *pane.terminal.lock() = term;
        for pattern in [
            Pattern::CaseSensitiveString("FOUND".into()),
            Pattern::Regex("FOUND".into()),
        ] {
            let found =
                promise::spawn::block_on(pane.search(pattern.clone(), 0..4100, Some(1))).unwrap();
            assert_eq!(
                found.len(),
                1,
                "a satisfied limit must stop before unrelated incomplete context"
            );
            assert_eq!((found[0].start_y, found[0].start_x), (0, 0));
            let more = promise::spawn::block_on(pane.search(pattern.clone(), 0..4100, Some(2)));
            if matches!(pattern, Pattern::Regex(_)) {
                assert!(more.is_err());
            } else {
                assert_eq!(more.unwrap().len(), 1);
            }
        }
        for pattern in [
            Pattern::CaseSensitiveString("XY".into()),
            Pattern::CaseInSensitiveString("xy".into()),
        ] {
            let found = promise::spawn::block_on(pane.search(pattern, 0..4100, Some(10))).unwrap();
            assert_eq!(found.len(), 1);
            assert_eq!((found[0].start_y, found[0].start_x), (3502, 0));
            assert_eq!((found[0].end_y, found[0].end_x), (3502, 2));
        }
        {
            let error = promise::spawn::block_on(pane.search(
                Pattern::Regex("XY".into()),
                0..4100,
                Some(10),
            ))
            .expect_err("truncated logical context must not claim complete search");
            assert!(error.to_string().contains("captured suffix context"));
        }
        // Both sides of a streaming capture boundary are genuinely spilled,
        // inside one logical paragraph longer than the former context cap.
        let sink = Arc::new(ColdResizeTestSink::default());
        let mut term = Terminal::new(
            term_size(4, 2),
            Arc::new(BoundaryConfig(
                ColdResizeTestConfig(Arc::clone(&sink)),
                4096,
            )),
            "FrankenTerm",
            "streaming-cold-boundary",
            Box::new(Vec::new()),
        );
        term.advance_bytes(
            format!(
                "{}İY{}\r\nnext\r\nnext\r\n",
                "a".repeat(3999),
                "b".repeat(10_000)
            )
            .as_bytes(),
        );
        assert!(term.screen().phys_to_stable_row_index(0) > 1000);
        assert!(sink.rows.lock().1.contains_key(&999));
        assert!(sink.rows.lock().1.contains_key(&1000));
        *pane.terminal.lock() = term;
        for pattern in [
            Pattern::CaseSensitiveString("İY".into()),
            Pattern::CaseInSensitiveString("i\u{307}y".into()),
        ] {
            let found = promise::spawn::block_on(pane.search(
                pattern,
                StableRowIndex::MIN..StableRowIndex::MAX,
                Some(10),
            ))
            .unwrap();
            assert_eq!(found.len(), 1);
            assert_eq!((found[0].start_y, found[0].start_x), (999, 3));
            assert_eq!((found[0].end_y, found[0].end_x), (1000, 1));
        }
        assert!(sink.payload_reads.load(Ordering::Acquire) > 0);
        let mut term = Terminal::new(
            term_size(4, 2),
            Arc::new(ColdResizeTestConfig(
                Arc::new(ColdResizeTestSink::default()),
            )),
            "FrankenTerm",
            "search-folded",
            Box::new(Vec::new()),
        );
        term.advance_bytes("İZ".as_bytes());
        *pane.terminal.lock() = term;
        let matches = promise::spawn::block_on(pane.search(
            Pattern::CaseInSensitiveString("z".into()),
            0..2,
            Some(10),
        ))
        .unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!((matches[0].start_y, matches[0].start_x), (0, 1));
        assert_eq!((matches[0].end_y, matches[0].end_x), (0, 2));
        pane.terminal.lock().advance_bytes("界".as_bytes());
        let matches = promise::spawn::block_on(pane.search(
            Pattern::CaseSensitiveString("界".into()),
            0..2,
            Some(10),
        ))
        .unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!((matches[0].start_y, matches[0].start_x), (0, 2));
        assert_eq!((matches[0].end_y, matches[0].end_x), (0, 4));
        let matches =
            promise::spawn::block_on(pane.search(Pattern::Regex("^|界".into()), 0..2, Some(1)))
                .unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!((matches[0].start_x, matches[0].end_x), (2, 4));
        let mut term = Terminal::new(
            term_size(4, 2),
            Arc::new(ColdResizeTestConfig(
                Arc::new(ColdResizeTestSink::default()),
            )),
            "FrankenTerm",
            "search-combining-grapheme",
            Box::new(Vec::new()),
        );
        term.advance_bytes("e\u{301}Z".as_bytes());
        *pane.terminal.lock() = term;
        for pattern in [
            Pattern::CaseSensitiveString("\u{301}".into()),
            Pattern::Regex("\u{301}".into()),
        ] {
            let found = promise::spawn::block_on(pane.search(pattern, 0..2, Some(10))).unwrap();
            assert_eq!(found.len(), 1);
            assert_eq!((found[0].start_y, found[0].start_x), (0, 0));
            assert_eq!((found[0].end_y, found[0].end_x), (0, 1));
        }
    }

    #[test]
    fn streaming_literal_rejects_gaps_cancellation_and_oversized_queries() {
        let pattern = Pattern::CaseSensitiveString("AB".into());
        let cancelled = AtomicBool::new(false);
        let mut unique = SearchMatchSet::default();
        let mut first = Line::from_text("A", &termwiz::cell::CellAttributes::blank(), 1, None);
        first.set_last_cell_was_wrapped(true, 1);
        let second = Line::from_text("B", &termwiz::cell::CellAttributes::blank(), 1, None);
        let mut stream = StreamingLiteralSearch::new(&pattern, 0..2, 0)
            .unwrap()
            .unwrap();
        assert!(stream
            .consume(0..1, &[&first], 10, &mut unique, &cancelled)
            .unwrap()
            .is_empty());
        assert!(stream
            .consume(2..3, &[&second], 10, &mut unique, &cancelled)
            .unwrap_err()
            .to_string()
            .contains("discontinuity"));
        assert!(stream
            .consume(1..3, &[&second], 10, &mut unique, &cancelled)
            .unwrap_err()
            .to_string()
            .contains("row gap"));
        cancelled.store(true, Ordering::Release);
        assert!(stream
            .consume(1..2, &[&second], 10, &mut unique, &cancelled)
            .unwrap_err()
            .to_string()
            .contains("cancelled"));
        cancelled.store(false, Ordering::Release);
        let found = stream
            .consume(1..2, &[&second], 10, &mut unique, &cancelled)
            .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(
            (
                found[0].start_y,
                found[0].start_x,
                found[0].end_y,
                found[0].end_x
            ),
            (0, 0, 1, 1)
        );
        let mut prefix = StreamingLiteralSearch::new(&pattern, 1..2, 0)
            .unwrap()
            .unwrap();
        cancelled.store(true, Ordering::Release);
        assert!(prefix
            .consume(0..1, &[&first], 10, &mut unique, &cancelled)
            .unwrap_err()
            .to_string()
            .contains("cancelled"));
        let maximum = frankenterm_term::screen::ScreenLineRead::MAX_PAYLOAD_BYTES
            / (2 * std::mem::size_of::<LiteralByteCoordinate>() + std::mem::size_of::<usize>() + 4);
        assert!(StreamingLiteralSearch::new(
            &Pattern::CaseSensitiveString("a".repeat(maximum + 1)),
            0..2,
            0
        )
        .is_err());
    }

    #[test]
    fn search_unique_match_budget_is_retained_across_chunks() {
        let mut unique = SearchMatchSet::default();
        let retained = "x".repeat(frankenterm_term::screen::ScreenLineRead::MAX_PAYLOAD_BYTES - 1);
        assert_eq!(unique.id_for(&retained).unwrap(), 0);
        drop(retained);
        let lines: Vec<_> = ["A", "B", "A"]
            .iter()
            .map(|text| Line::from_text(text, &termwiz::cell::CellAttributes::blank(), 1, None))
            .collect();
        let physical: Vec<_> = lines.iter().collect();
        let cancelled = AtomicBool::new(false);
        let first = search_owned_lines(
            Pattern::Regex(".".into()),
            0..1,
            1,
            (0, &physical),
            &(0..3),
            &mut unique,
            &cancelled,
        )
        .unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(
            unique.retained_bytes,
            frankenterm_term::screen::ScreenLineRead::MAX_PAYLOAD_BYTES
        );
        let error = search_owned_lines(
            Pattern::Regex(".".into()),
            1..2,
            1,
            (0, &physical),
            &(0..3),
            &mut unique,
            &cancelled,
        )
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("unique match text exceeds memory budget"));
        assert_eq!(unique.values.len(), 2);
        let repeated = search_owned_lines(
            Pattern::Regex(".".into()),
            2..3,
            1,
            (0, &physical),
            &(0..3),
            &mut unique,
            &cancelled,
        )
        .unwrap();
        assert_eq!(repeated.len(), 1);
        assert_eq!(repeated[0].match_id, first[0].match_id);
        assert_eq!((repeated[0].start_y, repeated[0].start_x), (2, 0));
        assert_eq!(
            unique.retained_bytes,
            frankenterm_term::screen::ScreenLineRead::MAX_PAYLOAD_BYTES
        );
        cancelled.store(true, Ordering::Release);
        assert!(search_owned_lines(
            Pattern::Regex(".".into()),
            2..3,
            1,
            (0, &physical),
            &(0..3),
            &mut unique,
            &cancelled
        )
        .is_err());
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
                pane.perform_actions(staged).unwrap();
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
    fn native_identity_publication_returns_fresh_authority_and_rejects_stale_intent() {
        let sink = Arc::new(ColdResizeTestSink::default());
        sink.witness_admission.store(true, Ordering::Relaxed);
        let mut terminal = Terminal::new(
            term_size(20, 2),
            Arc::new(ColdResizeTestConfig(sink.clone())),
            "FrankenTerm",
            "native-identity-publication",
            Box::new(Vec::new()),
        );
        terminal.advance_bytes(b"first\r\nsecond\r\nthird\r\nfourth\r\nfifth\r\n");
        let pane = LocalPane::new(
            721,
            terminal,
            Box::new(KillCountingChild {
                kills: Arc::new(AtomicUsize::new(0)),
            }),
            Box::new(GuardianLifetimeTestMasterPty),
            Box::new(Vec::<u8>::new()),
            1,
            [0x81; 16],
            "native-identity-publication".to_string(),
        );
        let first = *sink.rows.lock().1.first_key_value().unwrap().0;
        let (floor, sequence, dimensions) = pane.selection_source_snapshot().unwrap();
        let read = pane
            .capture_line_read(first..first + 1, &mut Default::default())
            .unwrap()
            .unwrap()
            .with_requested_physical_rows_only()
            .hydrate(|| false)
            .unwrap();
        assert!(pane
            .terminal
            .lock()
            .screen()
            .line_read_changes_layout(&read));
        let reads = std::slice::from_ref(&read);
        {
            let _busy = pane.terminal.lock();
            assert_eq!(
                pane.publish_line_reads_at_unchanged_coordinates(
                    reads,
                    floor,
                    sequence,
                    dimensions,
                    &mut |_, _, _| { panic!("busy identity publication") }
                ),
                Err(frankenterm_term::screen::ColdReadMetadataBusy)
            );
        }
        let mut wrong_retention = dimensions;
        wrong_retention.scrollback_top += 1;
        assert!(!pane
            .publish_line_reads_at_unchanged_coordinates(
                reads,
                floor,
                sequence,
                wrong_retention,
                &mut |_, _, _| { panic!("wrong retention accepted") }
            )
            .unwrap());
        let mut published = None;
        assert!(pane
            .publish_line_reads_at_unchanged_coordinates(
                reads,
                floor,
                sequence,
                dimensions,
                &mut |floor, sequence, dimensions| {
                    assert!(pane.terminal.try_lock().is_none());
                    published = Some((floor, sequence, dimensions));
                }
            )
            .unwrap());
        let published = published.unwrap();
        assert_eq!(published.1, sequence + 1);
        assert_eq!(published.2, dimensions);
        assert_eq!(pane.selection_source_snapshot(), Some(published));
        assert!(!pane
            .publish_line_reads_at_unchanged_coordinates(
                reads,
                floor,
                sequence,
                dimensions,
                &mut |_, _, _| { panic!("previous authority accepted after publication") }
            )
            .unwrap());
        assert!(pane
            .publish_line_reads_at_unchanged_coordinates(
                reads,
                published.0,
                published.1,
                published.2,
                &mut |f, s, d| {
                    assert_eq!((f, s, d), published);
                }
            )
            .unwrap());
        pane.terminal.lock().resize(term_size(7, 2));
        let (floor, sequence, dimensions) = pane.selection_source_snapshot().unwrap();
        let reflow = pane
            .capture_line_read(first..first + 1, &mut Default::default())
            .unwrap()
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        assert!(pane
            .terminal
            .lock()
            .screen()
            .line_read_changes_layout(&reflow));
        assert!(!pane
            .publish_line_reads_at_unchanged_coordinates(
                std::slice::from_ref(&reflow),
                floor,
                sequence,
                dimensions,
                &mut |_, _, _| panic!("reflow reinterpreted a numeric action")
            )
            .unwrap());
        assert_eq!(
            pane.selection_source_snapshot(),
            Some((floor, sequence, dimensions))
        );
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
                .try_capture_render_frame(Some(NativeViewport::new(requested)), 0, 0, &[], false)
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
    fn native_viewport_retains_logical_offset_across_repeated_reflow() {
        let pane = LocalPane::new(
            709,
            guardian_lifetime_test_terminal(),
            Box::new(KillCountingChild {
                kills: Arc::new(AtomicUsize::new(0)),
            }),
            Box::new(GuardianLifetimeTestMasterPty),
            Box::new(Vec::<u8>::new()),
            1,
            [0x79; 16],
            "native-viewport-anchor".to_string(),
        );
        {
            let mut term = pane.terminal.lock();
            term.resize(term_size(20, 4));
            for index in 0..40 {
                term.advance_bytes(
                    format!("abcde\u{301}fghij界klmnopqrUVWXYZ{index:02}\r\n").as_bytes(),
                );
            }
        }
        let dimensions = pane.get_dimensions();
        let (first, lines) = pane.get_lines(dimensions.scrollback_top..dimensions.physical_top);
        let offset = lines
            .iter()
            .position(|line| line.as_str() == "UVWXYZ10")
            .unwrap();
        let original_row = first + offset as StableRowIndex;
        let original = pane
            .try_capture_render_frame(Some(NativeViewport::new(original_row)), 0, 0, &[], false)
            .unwrap();
        assert_eq!(original.lines[0].as_str(), "UVWXYZ10");
        let mut viewport = original.viewport.unwrap();
        assert!(viewport.anchor.is_some());
        for (cols, expected) in [
            (13, "lmnopqrUVWXYZ"),
            (40, "abcde\u{301}fghij界klmnopqrUVWXYZ10"),
            (20, "UVWXYZ10"),
        ] {
            pane.terminal.lock().resize(term_size(cols, 4));
            {
                let _busy = pane.terminal.lock();
                assert!(pane
                    .try_capture_render_frame(Some(viewport.clone()), 0, 0, &[], false)
                    .is_none());
            }
            let frame = pane
                .try_capture_render_frame(Some(viewport), 0, 0, &[], false)
                .unwrap();
            assert_eq!(frame.lines[0].as_str(), expected);
            viewport = frame.viewport.unwrap();
            assert!(viewport.anchor.is_some());
            if cols == 13 {
                let numeric = pane
                    .try_capture_render_frame(
                        Some(NativeViewport::new(original_row)),
                        0,
                        0,
                        &[],
                        false,
                    )
                    .unwrap();
                assert_ne!(
                    numeric.lines[0].as_str(),
                    expected,
                    "numeric-only viewport must expose the original location-jump defect"
                );
            }
        }
        let bottom = pane
            .try_capture_render_frame(None, 0, 0, &[], false)
            .unwrap();
        assert!(bottom.viewport.is_none());
        assert_eq!(bottom.first, bottom.dimensions.physical_top);
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
        let mut retained: Vec<_> = (0..16)
            .map(|_| {
                pane.capture_selection_anchor(floor, sequence, dimensions, points)
                    .unwrap()
                    .expect("resident point must fill an available registry slot")
            })
            .collect();
        assert_eq!(
            pane.capture_selection_anchor(floor, sequence, dimensions, points),
            Err(SelectionAnchorCaptureError::Busy),
            "temporary registry capacity must keep selection capture retryable"
        );
        assert_eq!(retained.len(), 16);
        drop(retained.pop().unwrap());
        let recovered = pane
            .capture_selection_anchor(floor, sequence, dimensions, points)
            .unwrap()
            .expect("retiring a token must admit the same resident selection");
        assert_eq!(
            pane.selection_anchor_snapshot(&recovered).unwrap().3,
            Some(points)
        );
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
        use frankenterm_term::terminalstate::checkpoint::TerminalCheckpointV3;

        // Restore an otherwise valid real model near the sequence boundary;
        // no test-only setter or replacement terminal implementation is needed.
        let limits = TerminalCheckpointLimits::default();
        let checkpoint = guardian_lifetime_test_terminal()
            .capture_recovery_checkpoint(limits)
            .unwrap();
        let mut payload: serde_json::Value =
            serde_json::from_slice(checkpoint.canonical_payload()).unwrap();
        payload["seqno"] = serde_json::json!(SequenceNo::MAX - 3);
        let checkpoint: TerminalCheckpointV3 = serde_json::from_value(payload).unwrap();
        let encoded = checkpoint.to_canonical_json(limits).unwrap();
        let inert = TerminalCheckpointV3::decode_canonical_json(&encoded, limits)
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
    fn cold_viewport_anchor_survives_worker_hydration_and_resize() {
        const CHILD: &str = "FT_COLD_VIEWPORT_ANCHOR_CHILD";
        if std::env::var_os(CHILD).as_deref() != Some(std::ffi::OsStr::new("1")) {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "localpane::tests::cold_viewport_anchor_survives_worker_hydration_and_resize",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(45);
            loop {
                if child.try_wait().unwrap().is_some() {
                    let output = child.wait_with_output().unwrap();
                    assert!(
                        output.status.success(),
                        "{}{}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    );
                    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
                    return;
                }
                if Instant::now() >= deadline {
                    child.kill().unwrap();
                    let output = child.wait_with_output().unwrap();
                    panic!(
                        "cold viewport subprocess timed out: {}{}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    );
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        let executor = promise::spawn::SimpleExecutor::try_with_limits(
            promise::spawn::MainThreadAdmissionLimits::new(32, 512 * 1024, 0, 0).unwrap(),
        )
        .unwrap();
        let (pane, _mux, registration, _old_sink, token) = cold_resize_fixture(false);
        let sink = Arc::new(ColdResizeTestSink::default());
        let mut term = Terminal::new(
            term_size(20, 4),
            Arc::new(ColdResizeTestConfig(Arc::clone(&sink))),
            "FrankenTerm",
            "cold-viewport-anchor",
            Box::new(Vec::new()),
        );
        for index in 0..12 {
            term.advance_bytes(
                format!("abcde\u{301}fghij界klmnopqrUVWXYZ{index:02}\r\n").as_bytes(),
            );
        }
        *pane.terminal.lock() = term;
        LocalPane::prepare_cold_layout_after_resize(
            pane.pane_id(),
            &pane.terminal,
            &pane.line_layout_observation,
            &pane.resize_queue,
            token,
            registration.clone(),
        );
        let dimensions = pane.get_dimensions();
        let history = pane
            .capture_line_read(
                dimensions.scrollback_top..dimensions.physical_top,
                &mut Default::default(),
            )
            .unwrap()
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        let offset = history
            .lines()
            .position(|line| line.as_str() == "UVWXYZ04")
            .unwrap();
        let row = history.first_row() + offset as StableRowIndex;
        let capture = |viewport: NativeViewport| {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                while executor.try_tick().unwrap() {}
                if let Some(frame) =
                    pane.try_capture_render_frame(Some(viewport.clone()), 0, 0, &[], false)
                {
                    return frame;
                }
                assert!(
                    Instant::now() < deadline,
                    "cold viewport hydration did not converge"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
        };
        let frame = capture(NativeViewport::new(row));
        assert_eq!(frame.lines[0].as_str(), "UVWXYZ04");
        let mut viewport = frame.viewport.unwrap();
        assert!(
            viewport.cold_anchor.is_some(),
            "fixture must exercise cold identity"
        );
        // Exercise the real endpoint worker, not just Screen projection or a
        // fabricated completion. No selected-span payload cache is retained.
        COLD_VIEWPORT_CACHE.lock().clear();
        let (selection_floor, selection_sequence, selection_dimensions) =
            pane.selection_source_snapshot().unwrap();
        let selection_points = [
            Some(frankenterm_term::screen::SelectionAnchorCoordinate {
                row,
                column: Some(0),
            }),
            Some(frankenterm_term::screen::SelectionAnchorCoordinate {
                row,
                column: Some(0),
            }),
            Some(frankenterm_term::screen::SelectionAnchorCoordinate {
                row,
                column: Some(5),
            }),
        ];
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        *sink.read_gate.lock() = Some((entered_tx, release_rx));
        let first_capture = pane.capture_selection_anchor(
            selection_floor,
            selection_sequence,
            selection_dimensions,
            selection_points,
        );
        let entered = entered_rx.recv_timeout(Duration::from_secs(3));
        let first_cancelled = pane
            .cold_selection
            .lock()
            .first()
            .map(|work| Arc::clone(&work.cancelled));
        // A distinct viewport origin is requested while the actual endpoint
        // storage read is held. It must not cancel the selection's worker.
        let viewport_points = [selection_points[0], None, None];
        let competing = pane.capture_selection_anchor(
            selection_floor,
            selection_sequence,
            selection_dimensions,
            viewport_points,
        );
        let preserved = first_cancelled
            .as_ref()
            .is_some_and(|cancelled| !cancelled.load(Ordering::Acquire));
        let _ = release_tx.send(());
        sink.read_gate.lock().take();
        entered.expect("selection must enter the actual cold payload read");
        assert!(matches!(
            first_capture,
            Err(SelectionAnchorCaptureError::Busy)
        ));
        assert!(matches!(competing, Err(SelectionAnchorCaptureError::Busy)));
        assert!(
            preserved,
            "viewport capture cancelled an independently owned selection"
        );
        let mut viewport_token = None;
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut selection_token = None;
        loop {
            while executor.try_tick().unwrap() {}
            if viewport_token.is_none() {
                match pane.capture_selection_anchor(
                    selection_floor,
                    selection_sequence,
                    selection_dimensions,
                    viewport_points,
                ) {
                    Ok(Some(anchor)) => viewport_token = Some(anchor),
                    Err(SelectionAnchorCaptureError::Busy) => {}
                    other => panic!("cold viewport capture lost exact origin: {:?}", other),
                }
            }
            if selection_token.is_none() {
                match pane.capture_selection_anchor(
                    selection_floor,
                    selection_sequence,
                    selection_dimensions,
                    selection_points,
                ) {
                    Ok(Some(anchor)) => selection_token = Some(anchor),
                    Err(SelectionAnchorCaptureError::Busy) => {}
                    other => panic!("cold selection capture lost exact gesture: {:?}", other),
                }
            }
            if selection_token.is_some() && viewport_token.is_some() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "cold selection worker did not settle"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        let selection_anchor = selection_token.unwrap();
        let viewport_token = viewport_token.unwrap();
        assert!(pane.cold_selection.lock().iter().all(|work| {
            !matches!(
                work.ready.as_ref().map(|ready| &ready.value),
                Some(ColdSelectionValue::Captured(Ok(Some(_))))
            )
        }));
        // A superseded completion may only retire its own generation. This
        // exercises the production RAII guard used by canceled read workers.
        let old_cancelled = Arc::new(AtomicBool::new(true));
        let successor_cancelled = Arc::new(AtomicBool::new(false));
        let successor = Arc::new(Mutex::new(vec![ColdSelectionWork {
            request: ColdSelectionRequest::Resolve(selection_anchor.downgrade()),
            cancelled: Arc::clone(&successor_cancelled),
            ready: None,
            renewed: Instant::now(),
        }]));
        drop(ColdSelectionCompletion {
            slot: Arc::clone(&successor),
            cancelled: old_cancelled,
        });
        assert!(successor
            .lock()
            .first()
            .is_some_and(|work| Arc::ptr_eq(&work.cancelled, &successor_cancelled)));
        drop(ColdSelectionCompletion {
            slot: Arc::clone(&successor),
            cancelled: successor_cancelled,
        });
        assert!(successor.lock().is_empty());
        // Exercise admission on the real pane without sixteen redundant
        // storage workers. These seeded pending records test registry policy;
        // the two captures above supply actual worker/progress evidence.
        let retained = std::mem::take(&mut *pane.cold_selection.lock());
        let request = |column| ColdSelectionRequest::Capture {
            floor: selection_floor,
            sequence: selection_sequence,
            dimensions: selection_dimensions,
            points: [
                Some(frankenterm_term::screen::SelectionAnchorCoordinate {
                    row,
                    column: Some(column),
                }),
                None,
                None,
            ],
        };
        let cancelled: Vec<_> = (0..MAX_COLD_SELECTION_WORK)
            .map(|_| Arc::new(AtomicBool::new(false)))
            .collect();
        let admitted_at = Instant::now();
        *pane.cold_selection.lock() = cancelled
            .iter()
            .enumerate()
            .map(|(index, cancelled)| ColdSelectionWork {
                request: request(index + 1),
                cancelled: Arc::clone(cancelled),
                ready: None,
                renewed: admitted_at,
            })
            .collect();
        let reads_before = sink.payload_reads.load(Ordering::Acquire);
        {
            let mut term = pane.terminal.lock();
            pane.request_cold_selection(&mut term, request(0), vec![row..row + 1]);
            assert_eq!(pane.cold_selection.lock().len(), MAX_COLD_SELECTION_WORK);
            assert!(cancelled.iter().all(|flag| !flag.load(Ordering::Acquire)));
            assert_eq!(sink.payload_reads.load(Ordering::Acquire), reads_before);
            pane.cold_selection.lock()[0].renewed = admitted_at - Duration::from_secs(1);
            pane.request_cold_selection(&mut term, request(1), vec![row..row + 1]);
            assert!(pane.cold_selection.lock()[0].renewed >= admitted_at);
            assert_eq!(pane.cold_selection.lock().len(), MAX_COLD_SELECTION_WORK);
            // A cached SourceChanged result owns no payload/worker, but must
            // not occupy admission forever after the caller abandons it.
            let mut work = pane.cold_selection.lock();
            work[1].ready = Some(ColdSelectionReady {
                floor: selection_floor,
                sequence: selection_sequence,
                dimensions: selection_dimensions,
                value: ColdSelectionValue::Captured(Err(
                    SelectionAnchorCaptureError::SourceChanged,
                )),
            });
            work[1].renewed = Instant::now() - COLD_SELECTION_WORK_LEASE - Duration::from_secs(1);
            drop(work);
            pane.request_cold_selection(&mut term, request(0), vec![row..row + 1]);
            assert_eq!(pane.cold_selection.lock().len(), MAX_COLD_SELECTION_WORK);
            assert!(cancelled[1].load(Ordering::Acquire));
            assert!(cancelled
                .iter()
                .enumerate()
                .all(|(index, flag)| { index == 1 || !flag.load(Ordering::Acquire) }));
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            while executor.try_tick().unwrap() {}
            let ready = pane.cold_selection.lock().iter().any(|work| {
                work.request == request(0)
                    && matches!(
                        work.ready.as_ref().map(|ready| &ready.value),
                        Some(ColdSelectionValue::Captured(Ok(Some(_))))
                    )
            });
            if ready {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "reclaimed capacity must run the real cold read"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(sink.payload_reads.load(Ordering::Acquire) > reads_before);
        for work in std::mem::replace(&mut *pane.cold_selection.lock(), retained) {
            work.cancelled.store(true, Ordering::Release);
        }
        for (cols, expected) in [
            (13, "lmnopqrUVWXYZ"),
            (40, "abcde\u{301}fghij界klmnopqrUVWXYZ04"),
            (20, "UVWXYZ04"),
        ] {
            let size = term_size(cols, 4);
            let pty = pty_size(cols as u16, 4);
            let pending = {
                let mut queue = pane.resize_queue.lock();
                queue.enqueue(size, pty, Instant::now());
                queue.dequeue_for_worker().unwrap()
            };
            let token = ResizeCancellationToken::new(pending.seq);
            LocalPane::apply_resize_sync(
                pane.pane_id(),
                &pane.terminal,
                &pane.line_layout_observation,
                #[cfg(feature = "disruptor-pane-io")]
                &pane.action_ring,
                &pane.pty,
                &pane.resize_queue,
                pending.seq,
                size,
                pty,
                token,
            )
            .unwrap();
            LocalPane::prepare_cold_layout_after_resize(
                pane.pane_id(),
                &pane.terminal,
                &pane.line_layout_observation,
                &pane.resize_queue,
                token,
                registration.clone(),
            );
            let frame = capture(viewport);
            assert_eq!(frame.lines[0].as_str(), expected);
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut selection_resolved = None;
            let mut viewport_resolved = None;
            loop {
                while executor.try_tick().unwrap() {}
                if viewport_resolved.is_none() {
                    if let Some((_, _, _, points)) = pane.selection_anchor_snapshot(&viewport_token)
                    {
                        viewport_resolved =
                            Some(points.expect("cold viewport origin survives reflow"));
                    }
                }
                if selection_resolved.is_none() {
                    if let Some((_, _, _, points)) =
                        pane.selection_anchor_snapshot(&selection_anchor)
                    {
                        selection_resolved =
                            Some(points.expect("cold selection identity survives reflow"));
                    }
                }
                if selection_resolved.is_some() && viewport_resolved.is_some() {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "cold selection projection did not settle"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            let resolved = selection_resolved.unwrap();
            assert_eq!(viewport_resolved.unwrap(), [resolved[0], None, None]);
            let start_column = match cols {
                13 => 7,
                40 => 20,
                20 => 0,
                _ => unreachable!(),
            };
            assert_eq!(
                resolved[0],
                Some(frankenterm_term::screen::SelectionAnchorCoordinate {
                    row: frame.first,
                    column: Some(start_column),
                })
            );
            assert_eq!(resolved[1], resolved[0]);
            assert_eq!(
                resolved[2],
                Some(frankenterm_term::screen::SelectionAnchorCoordinate {
                    row: frame.first,
                    column: Some(start_column + 5),
                })
            );
            if cols != 20 {
                assert!(
                    matches!(
                        pane.capture_selection_anchor(
                            selection_floor,
                            selection_sequence,
                            selection_dimensions,
                            selection_points
                        ),
                        Err(SelectionAnchorCaptureError::SourceChanged)
                    ),
                    "a stale initial gesture must not become fresh after hydration"
                );
            }
            viewport = frame.viewport.unwrap();
            assert!(viewport.cold_anchor.is_some());
            if cols == 13 {
                // Ordinary output advances the cold frontier without issuing
                // a new resize. A stale layout must trigger its own hydration
                // instead of waiting forever for a nonexistent resize worker.
                pane.terminal.lock().advance_bytes(b"live\r\n");
                let frame = capture(viewport);
                assert_eq!(frame.lines[0].as_str(), expected);
                viewport = frame.viewport.unwrap();
                assert!(viewport.cold_anchor.is_some());
            }
        }
        let selection_weak = selection_anchor.downgrade();
        let viewport_weak = viewport_token.downgrade();
        drop(selection_anchor);
        drop(viewport_token);
        assert!(selection_weak.upgrade().is_none());
        assert!(viewport_weak.upgrade().is_none());
        cold_ragged_paragraph_selection_endpoints(&executor, false);
        cold_ragged_paragraph_selection_endpoints(&executor, true);
    }

    fn cold_ragged_paragraph_selection_endpoints(
        executor: &promise::spawn::SimpleExecutor,
        selection_before_index: bool,
    ) {
        use frankenterm_term::screen::SelectionAnchorCoordinate;
        let (pane, _mux, registration, _sink, token) = cold_resize_fixture(false);
        let sink = Arc::new(ColdResizeTestSink::default());
        sink.witness_admission
            .store(selection_before_index, Ordering::Release);
        let mut term = Terminal::new(
            term_size(80, 4),
            Arc::new(ColdResizeTestConfig(Arc::clone(&sink))),
            "FrankenTerm",
            "cold-ragged-selection",
            Box::new(Vec::new()),
        );
        let paragraph = format!(
            "RAGGED_04806 {}{} END_04806",
            "ab 界 e\u{301} 🚀 xy\u{a0}z ".repeat(7),
            "q".repeat(64)
        );
        assert_eq!(paragraph.len(), 241);
        term.advance_bytes(paragraph.as_bytes());
        term.advance_bytes(b"\r\n");
        for _ in 0..12 {
            term.advance_bytes(b"later unselected history\r\n");
        }
        if selection_before_index {
            // Leave an unselected logical line crossing the cold/hot seam.
            // Input then stays frozen throughout resize and selection work.
            term.advance_bytes("unselected soft tail ".repeat(40).as_bytes());
        }
        *pane.terminal.lock() = term;
        if !selection_before_index {
            LocalPane::prepare_cold_layout_after_resize(
                pane.pane_id(),
                &pane.terminal,
                &pane.line_layout_observation,
                &pane.resize_queue,
                token,
                registration.clone(),
            );
        }
        let dimensions = pane.get_dimensions();
        let read = pane
            .capture_line_read(
                dimensions.scrollback_top..dimensions.physical_top,
                &mut Default::default(),
            )
            .unwrap()
            .unwrap()
            .hydrate(|| false)
            .unwrap();
        let rows: Vec<_> = read.lines().collect();
        let first = rows
            .iter()
            .position(|line| line.as_str().starts_with("RAGGED_04806"))
            .unwrap();
        let last = rows
            .iter()
            .position(|line| line.as_str().contains("END_04806"))
            .unwrap();
        assert!(!rows[last].last_cell_was_wrapped());
        let end_len = rows[last].len();
        let start_row = read.first_row() + first as StableRowIndex;
        let end_row = read.first_row() + last as StableRowIndex;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            while executor.try_tick().unwrap() {}
            if let Some(frame) = pane.try_capture_render_frame(
                Some(NativeViewport::new(start_row)),
                0,
                0,
                &[],
                false,
            ) {
                assert!(frame.viewport.unwrap().cold_anchor.is_some());
                break;
            }
            assert!(
                Instant::now() < deadline,
                "ragged cold frame did not settle"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        let (floor, sequence, dimensions) = pane.selection_source_snapshot().unwrap();
        let mut anchors = Vec::new();
        for end_column in [end_len - 1, end_len, end_len + 1] {
            let origin = Some(SelectionAnchorCoordinate {
                row: start_row,
                column: Some(0),
            });
            let points = [
                origin,
                origin,
                Some(SelectionAnchorCoordinate {
                    row: end_row,
                    column: Some(end_column),
                }),
            ];
            COLD_VIEWPORT_CACHE.lock().clear();
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                while executor.try_tick().unwrap() {}
                match pane.capture_selection_anchor(floor, sequence, dimensions, points) {
                    Ok(Some(anchor)) => {
                        anchors.push(anchor);
                        break;
                    }
                    Err(SelectionAnchorCaptureError::Busy) => {}
                    result => panic!(
                        "ragged endpoint {} capture failed: {:?}",
                        end_column, result
                    ),
                }
                assert!(Instant::now() < deadline, "ragged capture did not settle");
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        for cols in [69, 106] {
            let size = term_size(cols, 4);
            let pty = pty_size(cols as u16, 4);
            let pending = {
                let mut queue = pane.resize_queue.lock();
                queue.enqueue(size, pty, Instant::now());
                queue.dequeue_for_worker().unwrap()
            };
            let token = ResizeCancellationToken::new(pending.seq);
            LocalPane::apply_resize_sync(
                pane.pane_id(),
                &pane.terminal,
                &pane.line_layout_observation,
                #[cfg(feature = "disruptor-pane-io")]
                &pane.action_ring,
                &pane.pty,
                &pane.resize_queue,
                pending.seq,
                size,
                pty,
                token,
            )
            .unwrap();
            if selection_before_index {
                // Production publishes primary geometry before preparing the
                // cold index. Deterministically run the actual selection read
                // worker in that gap, rather than relying on scheduling luck.
                COLD_VIEWPORT_CACHE.lock().clear();
                assert!(pane.selection_anchor_snapshot(&anchors[0]).is_none());
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    while executor.try_tick().unwrap() {}
                    let settled = pane
                        .cold_selection
                        .lock()
                        .iter()
                        .all(|work| work.ready.is_some());
                    if settled {
                        break;
                    }
                    assert!(Instant::now() < deadline, "selection worker did not settle");
                    std::thread::sleep(Duration::from_millis(1));
                }
                if let Some((_, _, _, points)) = pane.selection_anchor_snapshot(&anchors[0]) {
                    assert!(
                        points.is_some(),
                        "unpublished cold index must not permanently invalidate retained selection"
                    );
                }
            }
            LocalPane::prepare_cold_layout_after_resize(
                pane.pane_id(),
                &pane.terminal,
                &pane.line_layout_observation,
                &pane.resize_queue,
                token,
                registration.clone(),
            );
            for (extra, anchor) in anchors.iter().enumerate() {
                let deadline = Instant::now() + Duration::from_secs(5);
                let points = loop {
                    while executor.try_tick().unwrap() {}
                    if let Some((_, _, _, points)) = pane.selection_anchor_snapshot(anchor) {
                        break points.expect("ragged hard-ended selection survives resize");
                    }
                    assert!(
                        Instant::now() < deadline,
                        "ragged resolution did not settle"
                    );
                    std::thread::sleep(Duration::from_millis(1));
                };
                let start = points[1].unwrap();
                let end = points[2].unwrap();
                let read = pane
                    .capture_line_read(start.row..end.row + 1, &mut Default::default())
                    .unwrap()
                    .unwrap()
                    .hydrate(|| false)
                    .unwrap();
                let mut selected = String::new();
                let mut last_len = 0;
                for (offset, line) in read.lines().enumerate() {
                    let row = read.first_row() + offset as StableRowIndex;
                    if row < start.row || row > end.row {
                        continue;
                    }
                    for cell in line.visible_cells() {
                        if (row != start.row || cell.cell_index() >= start.column.unwrap())
                            && (row != end.row || cell.cell_index() <= end.column.unwrap())
                        {
                            selected.push_str(cell.str());
                        }
                    }
                    if row == end.row {
                        last_len = line.len();
                    }
                }
                assert_eq!(
                    selected.trim_end(),
                    paragraph,
                    "cols={cols} padding={extra}"
                );
                assert_eq!(end.column, Some(last_len - 1 + extra));
            }
        }
        if selection_before_index {
            let retained_selection: Vec<_> = sink
                .rows
                .lock()
                .1
                .range(..=end_row)
                .map(|(&row, line)| (row, line.clone()))
                .collect();
            assert!(!retained_selection.is_empty());
            // Advance the cold frontier without a resize or an index-preparation
            // call. Retire completed caches so the original tokens must drive
            // their own real-worker bootstrap against the new source.
            for work in pane.cold_selection.lock().drain(..) {
                assert!(work.ready.is_some());
                work.cancelled.store(true, Ordering::Release);
            }
            COLD_VIEWPORT_CACHE.lock().clear();
            pane.terminal
                .lock()
                .advance_bytes(b"\r\nunselected append one\r\nunselected append two");
            assert_eq!(
                sink.rows
                    .lock()
                    .1
                    .range(..=end_row)
                    .map(|(&row, line)| (row, line.clone()))
                    .collect::<Vec<_>>(),
                retained_selection,
                "unselected output must retain the exact selected source"
            );
            {
                let term = pane.terminal.lock();
                assert_eq!(
                    term.screen()
                        .selection_anchor_read_ranges(&anchors[0], term.current_seqno())
                        .unwrap(),
                    Some(std::iter::once(StableRowIndex::MIN..StableRowIndex::MIN + 1).collect()),
                    "output must invalidate the index and require a one-row bootstrap"
                );
            }
            for exhaust_optional_index in [false, true] {
                if exhaust_optional_index {
                    // A real optional index allocation refusal must not revoke
                    // source identity. Return to the original physical width
                    // so the bounded, unindexed payload fallback can succeed.
                    let apply = |cols| {
                        let size = term_size(cols, 4);
                        let pty = pty_size(cols as u16, 4);
                        let pending = {
                            let mut queue = pane.resize_queue.lock();
                            queue.enqueue(size, pty, Instant::now());
                            queue.dequeue_for_worker().unwrap()
                        };
                        let token = ResizeCancellationToken::new(pending.seq);
                        LocalPane::apply_resize_sync(
                            pane.pane_id(),
                            &pane.terminal,
                            &pane.line_layout_observation,
                            #[cfg(feature = "disruptor-pane-io")]
                            &pane.action_ring,
                            &pane.pty,
                            &pane.resize_queue,
                            pending.seq,
                            size,
                            pty,
                            token,
                        )
                        .unwrap();
                        token
                    };
                    apply(80);
                    for work in pane.cold_selection.lock().drain(..) {
                        assert!(work.ready.is_some());
                        work.cancelled.store(true, Ordering::Release);
                    }
                    COLD_VIEWPORT_CACHE.lock().clear();
                    let plan = pane
                        .terminal
                        .lock()
                        .screen()
                        .capture_line_read(StableRowIndex::MIN..StableRowIndex::MIN + 1)
                        .unwrap();
                    let failure = plan.failure_witness();
                    assert!(plan.hydrate_with_payload_limit(128, || false).is_err());
                    assert!(failure.retry_without_index());
                    let fallback = pane
                        .terminal
                        .lock()
                        .screen()
                        .capture_line_read(StableRowIndex::MIN..StableRowIndex::MIN + 1)
                        .unwrap();
                    let fallback = fallback
                        .hydrate_with_payload_limit(32 * 1024, || false)
                        .unwrap();
                    assert!(fallback
                        .capture_viewport_anchor(fallback.first_row())
                        .is_none());
                    {
                        let term = pane.terminal.lock();
                        assert!(term.screen().validates_line_read(&fallback));
                        assert!(!term.screen().line_read_changes_layout(&fallback));
                        assert_eq!(
                            term.screen()
                                .selection_anchor_read_ranges(&anchors[0], term.current_seqno())
                                .unwrap(),
                            Some(
                                std::iter::once(StableRowIndex::MIN..StableRowIndex::MIN + 1)
                                    .collect(),
                            )
                        );
                        assert!(term
                            .screen()
                            .resolve_selection_anchor_with_reads(
                                &anchors[0],
                                term.current_seqno(),
                                &[&fallback],
                            )
                            .unwrap()
                            .is_none());
                    }
                    let reads_before = sink.payload_reads.load(Ordering::Acquire);
                    let busy_before = sink.busy_observations.load(Ordering::Acquire);
                    let (entered_tx, entered_rx) = sync_channel(1);
                    let (release_tx, release_rx) = sync_channel(1);
                    assert!(sink.read_gate.lock().is_none());
                    *sink.read_gate.lock() = Some((entered_tx, release_rx));
                    let pending = pane.selection_anchor_snapshot(&anchors[0]);
                    // No terminal or selection lock is held while waiting for
                    // the real worker. Release and retire the gate before any
                    // assertion, including the timeout failure path.
                    let entered = entered_rx.recv_timeout(Duration::from_secs(3));
                    let _ = release_tx.send(());
                    sink.read_gate.lock().take();
                    assert!(pending.is_none());
                    entered.expect("selection worker must hydrate the bounded fallback");
                    let deadline = Instant::now() + Duration::from_secs(5);
                    loop {
                        while executor.try_tick().unwrap() {}
                        if pane
                            .cold_selection
                            .lock()
                            .iter()
                            .all(|work| work.ready.is_some())
                        {
                            break;
                        }
                        assert!(Instant::now() < deadline, "fallback worker did not settle");
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    assert!(sink.payload_reads.load(Ordering::Acquire) > reads_before);
                    assert_eq!(sink.busy_observations.load(Ordering::Acquire), busy_before);
                    if let Some((_, _, _, points)) = pane.selection_anchor_snapshot(&anchors[0]) {
                        assert!(
                            points.is_some(),
                            "missing projection must not revoke the token"
                        );
                    }
                    // A new geometry generation resets the optional index
                    // budget. Prove recovery with production preparation and
                    // the same tokens, rather than accepting indefinite Busy.
                    let token = apply(69);
                    LocalPane::prepare_cold_layout_after_resize(
                        pane.pane_id(),
                        &pane.terminal,
                        &pane.line_layout_observation,
                        &pane.resize_queue,
                        token,
                        registration.clone(),
                    );
                }
                for (extra, anchor) in anchors.iter().enumerate() {
                    let deadline = Instant::now() + Duration::from_secs(5);
                    let points = loop {
                        while executor.try_tick().unwrap() {}
                        if let Some((_, _, _, points)) = pane.selection_anchor_snapshot(anchor) {
                            break points
                                .expect("unselected output must preserve the original token");
                        }
                        assert!(
                            Instant::now() < deadline,
                            "selection must rebuild its index without a resize worker"
                        );
                        std::thread::sleep(Duration::from_millis(1));
                    };
                    let start = points[1].unwrap();
                    let end = points[2].unwrap();
                    assert_eq!(points[0], points[1]);
                    let read = pane
                        .capture_line_read(start.row..end.row + 1, &mut Default::default())
                        .unwrap()
                        .unwrap()
                        .hydrate(|| false)
                        .unwrap();
                    let mut selected = String::new();
                    let mut last_len = None;
                    for (offset, line) in read.lines().enumerate() {
                        let row = read.first_row() + offset as StableRowIndex;
                        if row < start.row || row > end.row {
                            continue;
                        }
                        for cell in line.visible_cells() {
                            if (row != start.row || cell.cell_index() >= start.column.unwrap())
                                && (row != end.row || cell.cell_index() <= end.column.unwrap())
                            {
                                selected.push_str(cell.str());
                            }
                        }
                        if row == end.row {
                            last_len = Some(line.len());
                        }
                    }
                    assert_eq!(selected.trim_end(), paragraph);
                    assert_eq!(end.column, Some(last_len.unwrap() - 1 + extra));
                }
            }
            // A one-shot storage/auth refusal does not prune any retained row.
            // Exercise the real worker with frozen source and the ORIGINAL
            // selection token, then require payload recovery without resize.
            for viewport in [false, true] {
                let deadline = Instant::now() + Duration::from_secs(5);
                let current_points = loop {
                    while executor.try_tick().unwrap() {}
                    if let Some((_, _, _, points)) = pane.selection_anchor_snapshot(&anchors[0]) {
                        break points.expect("original token must still resolve before refusal");
                    }
                    assert!(
                        Instant::now() < deadline,
                        "pre-refusal resolution did not settle"
                    );
                    std::thread::sleep(Duration::from_millis(1));
                };
                let viewport_row = current_points[1].unwrap().row;
                let expected_row = pane
                    .capture_line_read(viewport_row..viewport_row + 1, &mut Default::default())
                    .unwrap()
                    .unwrap()
                    .hydrate(|| false)
                    .unwrap()
                    .lines()
                    .next()
                    .unwrap()
                    .clone();
                for work in pane.cold_selection.lock().drain(..) {
                    assert!(work.ready.is_some());
                    work.cancelled.store(true, Ordering::Release);
                }
                COLD_VIEWPORT_CACHE.lock().clear();
                let before = sink.payload_refusals.load(Ordering::Acquire);
                let source_before = pane.selection_source_snapshot().unwrap();
                sink.refuse_next_payload.store(true, Ordering::Release);
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut recovered = false;
                let mut checked_backoff = false;
                loop {
                    while executor.try_tick().unwrap() {}
                    if !checked_backoff {
                        // Freeze the actual worker's failure deadline far in
                        // the future so admission assertions do not race a
                        // wall-clock sleep. Then expire that same record and
                        // require real payload recovery below.
                        let future = Instant::now() + Duration::from_secs(60);
                        let retained_failure = if viewport {
                            pane.cold_viewport_failure
                                .lock()
                                .as_mut()
                                .map(|failure| {
                                    failure.retry_at = future;
                                })
                                .is_some()
                        } else {
                            pane.cold_selection
                                .lock()
                                .first_mut()
                                .and_then(|work| work.ready.as_mut())
                                .is_some_and(|ready| {
                                    if let ColdSelectionValue::RetryAt(retry_at) = &mut ready.value
                                    {
                                        *retry_at = future;
                                        true
                                    } else {
                                        false
                                    }
                                })
                        };
                        if retained_failure {
                            let reads = sink.payload_reads.load(Ordering::Acquire);
                            for _ in 0..32 {
                                if viewport {
                                    assert!(pane
                                        .cold_viewport_lines(viewport_row..viewport_row + 1, None)
                                        .1
                                        .is_empty());
                                } else {
                                    assert!(pane.selection_anchor_snapshot(&anchors[0]).is_none());
                                }
                            }
                            assert_eq!(sink.payload_reads.load(Ordering::Acquire), reads);
                            let expired = Instant::now() - Duration::from_secs(1);
                            if viewport {
                                pane.cold_viewport_failure.lock().as_mut().unwrap().retry_at =
                                    expired;
                            } else {
                                pane.cold_selection
                                    .lock()
                                    .first_mut()
                                    .unwrap()
                                    .ready
                                    .as_mut()
                                    .unwrap()
                                    .value = ColdSelectionValue::RetryAt(expired);
                            }
                            checked_backoff = true;
                        }
                    }
                    if viewport {
                        let (first, lines) =
                            pane.cold_viewport_lines(viewport_row..viewport_row + 1, None);
                        if !lines.is_empty() {
                            assert_eq!(first, viewport_row);
                            assert_eq!(lines, vec![expected_row.clone()]);
                            recovered = true;
                        }
                    } else if let Some((_, _, _, points)) =
                        pane.selection_anchor_snapshot(&anchors[0])
                    {
                        let points = points.expect("transient read refusal revoked original token");
                        assert_eq!(points, current_points);
                        let start = points[1].unwrap();
                        let end = points[2].unwrap();
                        let read = pane
                            .capture_line_read(start.row..end.row + 1, &mut Default::default())
                            .unwrap()
                            .unwrap()
                            .hydrate(|| false)
                            .unwrap();
                        let mut selected = String::new();
                        for (offset, line) in read.lines().enumerate() {
                            let row = read.first_row() + offset as StableRowIndex;
                            if row < start.row || row > end.row {
                                continue;
                            }
                            for cell in line.visible_cells() {
                                if (row != start.row || cell.cell_index() >= start.column.unwrap())
                                    && (row != end.row || cell.cell_index() <= end.column.unwrap())
                                {
                                    selected.push_str(cell.str());
                                }
                            }
                        }
                        assert_eq!(selected.trim_end(), paragraph);
                        recovered = true;
                    }
                    if recovered {
                        break;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "retained cold source never recovered: viewport={}",
                        viewport
                    );
                    std::thread::sleep(Duration::from_millis(1));
                }
                assert!(
                    checked_backoff,
                    "worker refusal must retain bounded retry admission"
                );
                assert_eq!(sink.payload_refusals.load(Ordering::Acquire), before + 1);
                assert_eq!(pane.selection_source_snapshot().unwrap(), source_before);
            }
            // Remove the actual captured source rows from the fixture store.
            // A retryable layout failure must not make a genuinely pruned
            // selection immortal, including the last cached resolution.
            {
                let mut rows = sink.rows.lock();
                let before = rows.1.len();
                rows.1.retain(|row, _| *row > end_row);
                assert!(rows.1.len() < before);
                assert!(rows.1.first_key_value().unwrap().0 > &end_row);
            }
            for anchor in &anchors {
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    while executor.try_tick().unwrap() {}
                    if let Some((_, _, _, points)) = pane.selection_anchor_snapshot(anchor) {
                        assert!(points.is_none(), "pruned selection must be invalidated");
                        break;
                    }
                    assert!(Instant::now() < deadline, "pruned selection did not settle");
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
    }

    #[test]
    fn cold_viewport_anchor_prefetches_visible_context_without_another_frame() {
        const CHILD: &str = "FT_COLD_VIEWPORT_PREFETCH_CHILD";
        if std::env::var_os(CHILD).as_deref() != Some(std::ffi::OsStr::new("1")) {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "localpane::tests::cold_viewport_anchor_prefetches_visible_context_without_another_frame", "--nocapture"])
                .env(CHILD, "1")
                .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped())
                .spawn().unwrap();
            let deadline = Instant::now() + Duration::from_secs(45);
            loop {
                if child.try_wait().unwrap().is_some() {
                    let output = child.wait_with_output().unwrap();
                    assert!(
                        output.status.success(),
                        "{}{}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    );
                    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
                    return;
                }
                if Instant::now() >= deadline {
                    child.kill().unwrap();
                    let output = child.wait_with_output().unwrap();
                    panic!(
                        "prefetch test timed out: {}{}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    );
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        let executor = promise::spawn::SimpleExecutor::try_with_limits(
            promise::spawn::MainThreadAdmissionLimits::new(32, 512 * 1024, 0, 0).unwrap(),
        )
        .unwrap();
        for case in [
            "visible",
            "resize",
            "prune",
            "registration",
            "supersede",
            "busy",
            "panic",
        ] {
            let (pane, mux, registration, _, token) = cold_resize_fixture(false);
            let sink = Arc::new(ColdResizeTestSink::default());
            let mut terminal = Terminal::new(
                term_size(20, 4),
                Arc::new(ColdResizeTestConfig(sink.clone())),
                "FrankenTerm",
                "prefetch",
                Box::new(Vec::new()),
            );
            for index in 0..12 {
                terminal.advance_bytes(
                    format!("abcde\u{301}fghij界klmnopqrUVWXYZ{index:02}\r\n").as_bytes(),
                );
            }
            *pane.terminal.lock() = terminal;
            LocalPane::prepare_cold_layout_after_resize(
                pane.pane_id(),
                &pane.terminal,
                &pane.line_layout_observation,
                &pane.resize_queue,
                token,
                registration.clone(),
            );
            let dimensions = pane.get_dimensions();
            let history = pane
                .capture_line_read(
                    dimensions.scrollback_top..dimensions.physical_top,
                    &mut Default::default(),
                )
                .unwrap()
                .unwrap()
                .hydrate(|| false)
                .unwrap();
            assert!(pane
                .publish_line_reads(std::slice::from_ref(&history), &mut || {})
                .unwrap());
            let lines: Vec<_> = history
                .lines()
                .map(|line| line.as_str().into_owned())
                .collect();
            let offset = lines.iter().position(|line| line == "UVWXYZ04").unwrap();
            let row = history.first_row() + offset as StableRowIndex;
            let expected = lines[offset..offset + 4].to_vec();
            let anchor = history.capture_viewport_anchor(row).unwrap();
            let viewport = NativeViewport {
                row,
                anchor: None,
                cold_anchor: Some(anchor.clone()),
            };
            COLD_VIEWPORT_CACHE.lock().clear();
            // The anchor group ends before this row. Only the follow-up's
            // real payload read can reach this gate; no renderer polling.
            let anchor_range = pane
                .terminal
                .lock()
                .screen()
                .cold_viewport_anchor_read_range(&anchor)
                .unwrap()
                .unwrap();
            assert!(row + 2 >= anchor_range.end);
            assert!(sink.rows.lock().1.contains_key(&(row + 2)));
            if case == "busy" {
                let notifications = Arc::new(AtomicUsize::new(0));
                let counted = Arc::clone(&notifications);
                mux.subscribe(move |notification| {
                    if matches!(notification, crate::MuxNotification::PaneOutput(719)) {
                        counted.fetch_add(1, Ordering::Release);
                    }
                    true
                })
                .unwrap();
                let (request, requested) = sync_channel(1);
                let (locked, observed_lock) = sync_channel(1);
                let (release, released) = sync_channel(1);
                let terminal = Arc::clone(&pane.terminal);
                let holder = std::thread::spawn(move || {
                    requested.recv_timeout(Duration::from_secs(5)).unwrap();
                    let _guard = terminal.lock();
                    locked.send(()).unwrap();
                    released.recv_timeout(Duration::from_secs(5)).unwrap();
                });
                COLD_VIEWPORT_AFTER_PUBLISH.with(|hook| {
                    *hook.borrow_mut() = Some(Box::new(move || {
                        request.send(()).unwrap();
                        observed_lock.recv_timeout(Duration::from_secs(5)).unwrap();
                    }))
                });
                assert!(pane
                    .try_capture_render_frame(Some(viewport.clone()), 0, 0, &[], false)
                    .is_none());
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    while executor.try_tick().unwrap() {}
                    if pane.cold_viewport_pending.lock().is_none() {
                        break;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "busy continuation did not retire"
                    );
                    std::thread::sleep(Duration::from_millis(1));
                }
                release.send(()).unwrap();
                holder.join().unwrap();
                assert!(
                    notifications.load(Ordering::Acquire) > 0,
                    "accepted anchor must wake renderer when continuation is busy"
                );
                let deadline = Instant::now() + Duration::from_secs(5);
                let frame = loop {
                    while executor.try_tick().unwrap() {}
                    if let Some(frame) =
                        pane.try_capture_render_frame(Some(viewport.clone()), 0, 0, &[], false)
                    {
                        break frame;
                    }
                    assert!(Instant::now() < deadline, "normal fallback did not recover");
                    std::thread::sleep(Duration::from_millis(1));
                };
                assert_eq!(
                    frame
                        .lines
                        .iter()
                        .map(|line| line.as_str().into_owned())
                        .collect::<Vec<_>>(),
                    expected
                );
                continue;
            }
            let (entered, observed) = sync_channel(1);
            let (release, released) = sync_channel(1);
            *sink.read_gate_row.lock() = Some(row + 2);
            *sink.read_gate.lock() = Some((entered, released));
            assert!(pane
                .try_capture_render_frame(Some(viewport.clone()), 0, 0, &[], false)
                .is_none());
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                while executor.try_tick().unwrap() {}
                if observed.try_recv().is_ok() {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "{case}: visible read never started without a second frame",
                    case = case
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            let pending_token = Arc::clone(
                &pane
                    .cold_viewport_pending
                    .lock()
                    .as_ref()
                    .unwrap()
                    .cancelled,
            );
            let old_cache = Arc::clone(
                &COLD_VIEWPORT_CACHE
                    .lock()
                    .iter()
                    .find(|entry| entry.registration == registration.wire_identity())
                    .unwrap()
                    .read,
            );
            assert!(old_cache.cached_lines(row..row + 4).is_none());
            let retry_notifications = Arc::new(AtomicUsize::new(0));
            if case == "panic" {
                // The first publication wake has already been consumed while
                // pumping to the visible-only read gate. Count only later
                // notifications and do not poll the renderer to cause retries.
                let counted = Arc::clone(&retry_notifications);
                mux.subscribe(move |notification| {
                    if matches!(notification, crate::MuxNotification::PaneOutput(719)) {
                        counted.fetch_add(1, Ordering::Release);
                    }
                    true
                })
                .unwrap();
                sink.panic_next_payload.store(true, Ordering::Release);
            }
            let mut successor = None;
            match case {
                "resize" => pane.terminal.lock().resize(term_size(13, 4)),
                "prune" => sink.rows.lock().1.retain(|key, _| *key > row + 3),
                "registration" => {
                    assert!(registration.retire_if_current());
                    let dynamic: Arc<dyn Pane> = pane.clone();
                    let generation = crate::PaneRegistrationGeneration::new(
                        pane.pane_id(),
                        &mux.pane_retirements,
                        Arc::downgrade(&mux),
                    );
                    let deadline = Instant::now() + Duration::from_secs(5);
                    loop {
                        let inserted = {
                            let _guard = mux.pane_registration.lock();
                            mux.insert_pane_registration_locked(
                                pane.pane_id(),
                                pane.domain_id(),
                                &dynamic,
                                &generation,
                            )
                            .is_ok()
                        };
                        if inserted {
                            break;
                        }
                        while executor.try_tick().unwrap() {}
                        assert!(Instant::now() < deadline, "retirement fence did not settle");
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    assert_ne!(
                        mux.capture_pane_registration(&dynamic)
                            .unwrap()
                            .wire_identity(),
                        registration.wire_identity()
                    );
                }
                "supersede" => {
                    let cancelled = Arc::new(AtomicBool::new(false));
                    pending_token.store(true, Ordering::Release);
                    *pane.cold_viewport_pending.lock() = Some(ColdViewportPending {
                        requested: row + 8..row + 9,
                        cancelled: Arc::clone(&cancelled),
                    });
                    successor = Some(cancelled);
                }
                _ => {}
            }
            release.send(()).unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                while executor.try_tick().unwrap() {}
                // Only the worker's completion guard retains this token after
                // the pending slot is replaced. Waiting also exercises permit
                // lifetime rather than racing a still-unpublished completion.
                let done = if successor.is_some() {
                    Arc::strong_count(&pending_token) == 1
                } else {
                    pane.cold_viewport_pending.lock().is_none()
                };
                if done {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "prefetch completion did not retire: {}",
                    case
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            if case == "panic" {
                assert_eq!(sink.payload_panics.load(Ordering::Acquire), 1);
                let deadline = Instant::now() + Duration::from_secs(5);
                while retry_notifications.load(Ordering::Acquire) == 0 {
                    while executor.try_tick().unwrap() {}
                    assert!(
                        Instant::now() < deadline,
                        "contained follow-up panic lost retry wake"
                    );
                    std::thread::sleep(Duration::from_millis(1));
                }
                assert!(
                    pane.cold_viewport_failure.lock().is_some(),
                    "panic must enter ordinary read-failure handling"
                );
                let deadline = Instant::now() + Duration::from_secs(5);
                let frame = loop {
                    while executor.try_tick().unwrap() {}
                    if let Some(frame) =
                        pane.try_capture_render_frame(Some(viewport.clone()), 0, 0, &[], false)
                    {
                        break frame;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "contained follow-up panic never recovered"
                    );
                    std::thread::sleep(Duration::from_millis(1));
                };
                assert_eq!(
                    frame
                        .lines
                        .iter()
                        .map(|line| line.as_str().into_owned())
                        .collect::<Vec<_>>(),
                    expected
                );
                assert_eq!(sink.payload_panics.load(Ordering::Acquire), 1);
            } else if case == "visible" {
                let reads = sink.payload_reads.load(Ordering::Acquire);
                let frame = pane
                    .try_capture_render_frame(Some(viewport), 0, 0, &[], false)
                    .expect("visible context must already be cached");
                assert_eq!(
                    frame
                        .lines
                        .iter()
                        .map(|line| line.as_str().into_owned())
                        .collect::<Vec<_>>(),
                    expected
                );
                assert_eq!(
                    sink.payload_reads.load(Ordering::Acquire),
                    reads,
                    "final frame must not launch another read"
                );
            } else {
                assert!(
                    COLD_VIEWPORT_CACHE
                        .lock()
                        .iter()
                        .filter(|entry| entry.registration == registration.wire_identity())
                        .all(|entry| Arc::ptr_eq(&entry.read, &old_cache)),
                    "stale follow-up published: {}",
                    case
                );
            }
            if let Some(successor) = successor {
                let mut pending = pane.cold_viewport_pending.lock();
                assert!(Arc::ptr_eq(
                    &pending.as_ref().unwrap().cancelled,
                    &successor
                ));
                pending.take();
            }
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
            followup: None,
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
    fn guardian_publication_rejects_captured_owner_mismatch() {
        use crate::guardian_checkpoint::{
            GuardianSpawnCaptureProvenanceV1, GuardianSpawnCustodyScopeV1,
        };
        let identity = guardian_lifetime_test_identity(1);
        let control = Arc::new(FencedGuardianLeaseControl::new(identity));
        let mut pane =
            guardian_lifetime_test_pane(725, identity, control, Arc::new(AtomicUsize::new(0)));
        let provenance = GuardianSpawnCaptureProvenanceV1 {
            original: GuardianSpawnCustodyScopeV1 {
                broker_lineage: Uuid::from_u128(1),
                guardian_incarnation: identity.guardian_incarnation(),
                mux_incarnation: identity.mux_incarnation(),
                broker_build: [1; 32],
                guardian_build: [2; 32],
                mux_build: [3; 32],
                pane_id: identity.pane_id(),
                effect_id: Uuid::from_u128(2),
            },
            current_mux_incarnation: identity.mux_incarnation(),
            current_lease_generation: 1,
            acknowledged_successor: None,
        };
        for wrong_generation in [false, true] {
            let mut wrong = provenance;
            if wrong_generation {
                wrong.current_lease_generation = 2;
            } else {
                wrong.current_mux_incarnation = Uuid::new_v4();
            }
            let LocalPaneOwnership::Guardian(owner) = &mut pane.ownership else {
                unreachable!()
            };
            owner.spawn_custody = Some(wrong);
            assert!(pane.validate_unpublished_guardian_proxy().is_err());
        }
        let LocalPaneOwnership::Guardian(owner) = &mut pane.ownership else {
            unreachable!()
        };
        owner.spawn_custody = Some(provenance);
        pane.validate_unpublished_guardian_proxy().unwrap();
        assert_eq!(pane.guardian_spawn_custody(), Some(provenance));
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
        assert_eq!(next_search_grapheme_idx(0, 1), 1);
        assert_eq!(next_search_grapheme_idx(0, 2), 2);
        assert_eq!(next_search_grapheme_idx(usize::MAX, 2), usize::MAX);
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
            assert!(
                ring.push(AdmittedPaneActions {
                    actions: vec![Action::Print('x')],
                    alerts: None,
                    staging: Arc::new(Mutex::new(PaneAlertStaging::default())),
                })
                .is_ok(),
                "ring should accept unregistered model-only action"
            );

            let terminal = Mutex::new(test_terminal(size));
            let pty: Mutex<Box<dyn MasterPty>> = Mutex::new(Box::new(TestMasterPty));
            let resize_queue = Mutex::new(ResizeQueueState {
                pending: None,
                next_seq: 1,
                worker_running: true,
                reconcile_tab_on_completion: false,
                completion_reservation: None,
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
                    crate::guardian_checkpoint::LiveParserCheckpointSource::Output {
                        segment,
                        output: receipt,
                    },
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
                pane.perform_actions(staged).unwrap();
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
            TerminalCheckpointV3::decode_canonical_json(recovery.canonical_payload(), limits)
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
            TerminalCheckpointV3::decode_canonical_json(recovery.canonical_payload(), limits)
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
    fn metadata_refusal_logging_is_opt_in_bounded_and_outside_terminal_lock() {
        const CHILD: &str = "FT_METADATA_REFUSAL_LOG_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "localpane::tests::metadata_refusal_logging_is_opt_in_bounded_and_outside_terminal_lock",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn().unwrap();
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                if child.try_wait().unwrap().is_some() {
                    let output = child.wait_with_output().unwrap();
                    assert!(
                        output.status.success(),
                        "{}{}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    );
                    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
                    return;
                }
                if Instant::now() >= deadline {
                    child.kill().unwrap();
                    let _ = child.wait();
                    panic!("metadata diagnostic subprocess exceeded watchdog");
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        struct Recorder {
            terminal: Arc<Mutex<Terminal>>,
            emitted: AtomicUsize,
        }
        impl log::Log for Recorder {
            fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
                metadata.target() == "mux::metadata_refusal"
            }
            fn log(&self, record: &log::Record<'_>) {
                if self.enabled(record.metadata()) {
                    assert!(
                        self.terminal.try_lock().is_some(),
                        "logging held terminal lock"
                    );
                    let text = record.args().to_string();
                    assert!(text.starts_with("metadata_refusal_totals="));
                    assert!(text.contains("surface_tmux"));
                    assert!(!text.contains("DO_NOT_LOG_TERMINAL_CONTENT"));
                    self.emitted.fetch_add(1, Ordering::Relaxed);
                }
            }
            fn flush(&self) {}
        }
        let mut terminal = guardian_lifetime_test_terminal();
        terminal.advance_bytes(b"DO_NOT_LOG_TERMINAL_CONTENT");
        let pane = make_legacy_test_pane(783, terminal);
        let recorder = Box::leak(Box::new(Recorder {
            terminal: Arc::clone(&pane.terminal),
            emitted: AtomicUsize::new(0),
        }));
        log::set_logger(recorder).unwrap();
        log::set_max_level(log::LevelFilter::Off);
        let tmux_guard = pane.tmux_domain.lock();
        assert!(matches!(pane.capture_surface_snapshot(0), Some(Err(_))));
        assert_eq!(recorder.emitted.load(Ordering::Relaxed), 0);
        log::set_max_level(log::LevelFilter::Debug);
        for _ in 0..METADATA_REFUSAL_LOG_LIMIT + 32 {
            assert!(matches!(pane.capture_surface_snapshot(0), Some(Err(_))));
        }
        assert_eq!(
            recorder.emitted.load(Ordering::Relaxed),
            METADATA_REFUSAL_LOG_LIMIT
        );
        assert_eq!(
            METADATA_REFUSALS[MetadataRefusalStage::SurfaceTmux as usize].load(Ordering::Relaxed),
            METADATA_REFUSAL_LOG_LIMIT + 32
        );
        drop(tmux_guard);
        assert!(matches!(pane.capture_surface_snapshot(0), Some(Ok(_))));
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
            assert_eq!(
                LAST_METADATA_REFUSAL.with(|last| last.get()),
                Some(MetadataRefusalStage::SurfaceTerminal)
            );
            assert!(pane
                .capture_line_read(0..1, &mut Default::default())
                .unwrap()
                .is_err());
            assert_eq!(
                LAST_METADATA_REFUSAL.with(|last| last.get()),
                Some(MetadataRefusalStage::CaptureTerminal)
            );
            assert!(pane
                .publish_line_reads(&[], &mut || panic!("busy publication"))
                .is_err());
            assert_eq!(
                LAST_METADATA_REFUSAL.with(|last| last.get()),
                Some(MetadataRefusalStage::PublishTerminal)
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
            assert_eq!(
                LAST_METADATA_REFUSAL.with(|last| last.get()),
                Some(MetadataRefusalStage::SurfaceTmux)
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
            assert_eq!(
                LAST_METADATA_REFUSAL.with(|last| last.get()),
                Some(MetadataRefusalStage::LayoutObservation)
            );
        }

        // 4. Cold sink busy refusal
        {
            let (cold_pane, _mux, _reg, sink, _token) = cold_resize_fixture(false);
            sink.busy.store(true, Ordering::Release);
            assert!(matches!(
                cold_pane.capture_surface_snapshot(0),
                Some(Err(_))
            ));
            assert_eq!(
                LAST_METADATA_REFUSAL.with(|last| last.get()),
                Some(MetadataRefusalStage::SinkInterval)
            );
            sink.busy.store(false, Ordering::Release);
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
            assert_eq!(
                LAST_METADATA_REFUSAL.with(|last| last.get()),
                Some(MetadataRefusalStage::SinkUsage)
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
