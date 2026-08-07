use crate::domain::DomainId;
use crate::pane::{
    CachePolicy, CloseReason, ForEachPaneLogicalLine, LogicalLine, Pane, PaneId, Pattern,
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
use frankenterm_sigpipe::{catch_recoverable, RecoverablePanicSite};
use frankenterm_dynamic::Value;
use frankenterm_term::color::ColorPalette;
use frankenterm_term::{
    Alert, AlertHandler, Clipboard, DownloadHandler, KeyCode, KeyModifiers, MouseEvent, Progress,
    SemanticZone, StableRowIndex, Terminal, TerminalConfiguration, TerminalSize,
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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};
use termwiz::escape::csi::{Sgr, CSI};
use termwiz::escape::{Action, DeviceControlMode};
use termwiz::input::KeyboardEncoding;
use termwiz::surface::{Line, SequenceNo};
use url::Url;

#[cfg(feature = "disruptor-pane-io")]
use crossbeam::queue::ArrayQueue;

const PROC_INFO_CACHE_TTL: Duration = Duration::from_millis(300);

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
    next_intent: u64,
    completed_intent: u64,
    scheduled: bool,
    failed_registration: Option<PaneRegistrationHandle>,
}

impl ChildExitPruneTracker {
    fn record_child_exit(&mut self) {
        self.child_exited = true;
        self.next_intent = self.next_intent.saturating_add(1);
        self.failed_registration = None;
    }

    fn record_registration_bound(&mut self) -> bool {
        self.failed_registration = None;
        if !self.child_exited {
            return false;
        }
        self.next_intent = self.next_intent.saturating_add(1);
        true
    }

    fn has_pending_intent(&self) -> bool {
        self.completed_intent < self.next_intent
    }

    fn record_success(&mut self, target_intent: u64) {
        self.completed_intent = self.completed_intent.max(target_intent);
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
/// Intents are sequenced because the same pane object may be registered again
/// after its prior generation retires. A prune accepted for generation N must
/// not consume a concurrent bind intent for generation N+1.
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
            tracker.scheduled = true;
            tracker.next_intent
        };

        let dispatch = ChildExitPruneDispatch {
            state: Arc::clone(self),
            registration: Some(registration),
            target_intent,
            finished: false,
        };
        promise::spawn::spawn_into_main_thread(async move {
            dispatch.execute();
        })
        .detach();
    }

    fn finish_dispatch(
        self: &Arc<Self>,
        target_intent: u64,
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
    target_intent: u64,
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
            .finish_dispatch(self.target_intent, &registration, pruned);
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
    ) -> Result<ResizeEnqueueOutcome, ResizeEnqueueError> {
        let seq = self
            .next_seq
            .checked_add(1)
            .ok_or(ResizeEnqueueError::SequenceExhausted)?;
        let replaced_seq = self.pending.as_ref().map(|pending| pending.seq);
        let spawn_worker = !self.worker_running;
        let queue_depth_hint = if self.worker_running { 2 } else { 1 };

        self.next_seq = seq;
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
        self.try_enqueue(size, pty_size, enqueued_at)
            .expect("test resize generation must remain below u64::MAX")
    }

    fn dequeue_for_worker(&mut self) -> Option<PendingResize> {
        if let Some(pending) = self.pending.take() {
            return Some(pending);
        }

        self.worker_running = false;
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
        Err(_) => Err(
            resize_queue
                .lock()
                .recover_failed_intent(pending, ResizeFailureKind::RecoverablePanic),
        ),
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

fn retry_with_backoff<T, E, F>(
    policy: ResizeRetryPolicy,
    mut op: F,
) -> Result<(T, ResizeRetryStats), (E, ResizeRetryStats)>
where
    F: FnMut(usize) -> Result<T, E>,
{
    retry_with_backoff_controlled(policy, |attempt| {
        op(attempt).map_err(RetryStepError::Retry)
    })
}

pub struct LocalPane {
    pane_id: PaneId,
    terminal: Arc<Mutex<Terminal>>,
    process: Arc<Mutex<ProcessState>>,
    pty: Arc<Mutex<Box<dyn MasterPty>>>,
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
    fn pane_id(&self) -> PaneId {
        self.pane_id
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

    fn for_each_logical_line_in_stable_range_mut(
        &self,
        lines: Range<StableRowIndex>,
        for_line: &mut dyn ForEachPaneLogicalLine,
    ) {
        terminal_for_each_logical_line_in_stable_range_mut(
            &mut self.locked_terminal(),
            lines,
            for_line,
        );
    }

    fn with_lines_mut(&self, lines: Range<StableRowIndex>, with_lines: &mut dyn WithPaneLines) {
        terminal_with_lines_mut(&mut self.locked_terminal(), lines, with_lines)
    }

    fn get_lines(&self, lines: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
        terminal_get_lines(&mut self.locked_terminal(), lines)
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
        self.locked_terminal().set_config(config);
    }

    fn get_config(&self) -> Option<Arc<dyn TerminalConfiguration>> {
        Some(self.locked_terminal().get_config())
    }

    fn perform_actions(&self, actions: Vec<termwiz::escape::Action>) {
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
        self.enqueue_resize(size)
    }

    fn writer(&self) -> MappedMutexGuard<'_, dyn std::io::Write> {
        record_input_for_current_identity(&self.mux_registration);
        MutexGuard::map(self.writer.lock(), |writer| {
            let w: &mut dyn std::io::Write = writer;
            w
        })
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
        // If the title is the default pane title, then try to spice
        // things up a bit by returning the process basename instead
        if title == "wezterm" {
            if let Some(proc_name) = self.get_foreground_process_name(CachePolicy::AllowStale) {
                let proc_name = std::path::Path::new(&proc_name);
                if let Some(name) = proc_name.file_name() {
                    return name.to_string_lossy().to_string();
                }
            }
        }

        title
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
                    for capture_res in re.captures_iter(&haystack) {
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

pub(crate) fn emit_output_for_pane(registration: PaneRegistrationHandle, message: &str) {
    let mut parser = termwiz::escape::parser::Parser::new();
    let mut actions = vec![Action::CSI(CSI::Sgr(Sgr::Reset))];
    parser.parse(message.as_bytes(), |action| actions.push(action));

    if promise::spawn::is_scheduler_configured() {
        promise::spawn::spawn_into_main_thread(async move {
            let _ = registration.try_with_current_output(|pane| {
                pane.perform_actions(actions);
            });
        })
        .detach();
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

struct LocalPaneNotifHandler {
    pane_id: PaneId,
    mux_registration: Arc<PaneRegistrationSlot>,
}

impl AlertHandler for LocalPaneNotifHandler {
    fn alert(&mut self, alert: Alert) {
        if !promise::spawn::is_scheduler_configured() {
            return;
        }
        let Some(registration) = self.mux_registration.load() else {
            log::trace!(
                "dropping alert for unregistered local pane {}",
                self.pane_id
            );
            return;
        };
        promise::spawn::spawn_into_main_thread(async move {
            let _ = registration.try_with_current(|pane| {
                pane.dispatch_alert(alert);
            });
        })
        .detach();
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

    fn enqueue_resize(&self, size: TerminalSize) -> Result<(), Error> {
        let pty_size = PtySize {
            rows: size.rows.try_into()?,
            cols: size.cols.try_into()?,
            pixel_width: size.pixel_width.try_into()?,
            pixel_height: size.pixel_height.try_into()?,
        };
        let enqueued_at = Instant::now();

        let outcome = match {
            let mut queue = self.resize_queue.lock();
            queue.try_enqueue(size, pty_size, enqueued_at)
        } {
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
                #[cfg(feature = "disruptor-pane-io")]
                Arc::clone(&self.action_ring),
                Arc::clone(&self.pty),
                Arc::clone(&self.resize_queue),
            );
        }

        Ok(())
    }

    fn spawn_resize_worker(
        pane_id: PaneId,
        terminal: Arc<Mutex<Terminal>>,
        #[cfg(feature = "disruptor-pane-io")] action_ring: Arc<ArrayQueue<Vec<Action>>>,
        pty: Arc<Mutex<Box<dyn MasterPty>>>,
        resize_queue: Arc<Mutex<ResizeQueueState>>,
    ) {
        let worker_terminal = Arc::clone(&terminal);
        #[cfg(feature = "disruptor-pane-io")]
        let worker_action_ring = Arc::clone(&action_ring);
        let worker_pty = Arc::clone(&pty);
        let worker_queue = Arc::clone(&resize_queue);
        let spawn_result = std::thread::Builder::new()
            .name(format!("pane-resize-{}", pane_id))
            .spawn(move || {
                Self::run_resize_worker(
                    pane_id,
                    worker_terminal,
                    #[cfg(feature = "disruptor-pane-io")]
                    worker_action_ring,
                    worker_pty,
                    worker_queue,
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
                #[cfg(feature = "disruptor-pane-io")]
                action_ring,
                pty,
                resize_queue,
            );
        });
    }

    fn run_resize_worker(
        pane_id: PaneId,
        terminal: Arc<Mutex<Terminal>>,
        #[cfg(feature = "disruptor-pane-io")] action_ring: Arc<ArrayQueue<Vec<Action>>>,
        pty: Arc<Mutex<Box<dyn MasterPty>>>,
        resize_queue: Arc<Mutex<ResizeQueueState>>,
    ) {
        while let Some(pending) = {
            let mut queue = resize_queue.lock();
            queue.dequeue_for_worker()
        } {
            let queue_wait = pending.enqueued_at.elapsed();
            let completion_start = Instant::now();
            let token = ResizeCancellationToken::new(pending.seq);
            let apply_result = catch_resize_intent(resize_queue.as_ref(), pending, || {
                Self::apply_resize_sync(
                    pane_id,
                    terminal.as_ref(),
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
            let settled_apply_result = apply_result.map(|result| {
                recover_resize_apply_error(resize_queue.as_ref(), pending, result)
            });
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

    fn apply_resize_sync(
        pane_id: PaneId,
        terminal: &Mutex<Terminal>,
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
                    return Err(RetryStepError::Stop(
                        PtyResizeAttemptFailure::Superseded { by_seq },
                    ));
                }
                let pty_lock_start = Instant::now();
                let pty = pty.lock();
                pty_lock_wait += pty_lock_start.elapsed();
                if let Some(by_seq) = resize_queue.lock().superseded_by(token) {
                    drop(pty);
                    return Err(RetryStepError::Stop(
                        PtyResizeAttemptFailure::Superseded { by_seq },
                    ));
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
                    return Err(RetryStepError::Retry(PtyResizeAttemptFailure::Apply(
                        err,
                    )));
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

        let terminal_apply_lock_start = Instant::now();
        let mut terminal = terminal.lock();
        #[cfg(feature = "disruptor-pane-io")]
        Self::drain_action_ring_into(action_ring, &mut terminal);
        let terminal_apply_lock_wait = terminal_apply_lock_start.elapsed();
        let (commit_decision, swap_barrier_wait) = with_resize_commit_barrier(
            resize_queue,
            token,
            || {
                if terminal.get_size() == size {
                    return Duration::default();
                }
                let terminal_resize_start = Instant::now();
                terminal.resize(size);
                terminal_resize_start.elapsed()
            },
        );
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
        mut terminal: Terminal,
        process: Box<dyn Child + Send>,
        pty: Box<dyn MasterPty>,
        writer: Box<dyn Write + Send>,
        domain_id: DomainId,
        command_description: String,
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

        Self {
            pane_id,
            terminal: Arc::new(Mutex::new(terminal)),
            process: Arc::clone(&process),
            pty: Arc::new(Mutex::new(pty)),
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
        let tmux_domain = self.tmux_domain.lock().take();
        if let Some(tmux) = tmux_domain {
            // Eagerly tear down tmux-domain state if this pane is being dropped
            // without a clean control-mode exit sequence.
            tmux.transition_to_exit_and_schedule_detach();
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
        let post_bind_intent = tracker.next_intent;
        tracker.record_success(post_bind_intent);
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
        let first_generation_intent = tracker.next_intent;

        assert!(tracker.record_registration_bound());
        let replacement_generation_intent = tracker.next_intent;
        tracker.record_success(first_generation_intent);

        assert!(
            tracker.has_pending_intent(),
            "completion for an old generation must preserve a newer bind intent"
        );
        tracker.record_success(replacement_generation_intent);
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
            target_intent: 1,
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
                    queue.lock().enqueue(
                        term_size(120, 40),
                        pty_size(120, 40),
                        Instant::now(),
                    );
                    return Err(RetryStepError::Retry("transient apply failure"));
                }
                Ok(())
            });

        let (err, stats) = result.expect_err("newer intent must stop the stale retry loop");
        assert_eq!(err, "superseded");
        assert_eq!(stats.attempts, 2);
        assert_eq!(simulated_pty_calls, 1);
        assert_eq!(queue.lock().pending.as_ref().map(|intent| intent.seq), Some(2));
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

        let second = queue.enqueue(term_size(100, 30), pty_size(100, 30), now);
        assert_eq!(second.seq, 2);
        assert!(!second.spawn_worker);
        assert_eq!(second.replaced_seq, None);
        assert_eq!(second.queue_depth_hint, 2);

        let third = queue.enqueue(term_size(120, 40), pty_size(120, 40), now);
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
            queue.try_enqueue(term_size(120, 40), pty_size(120, 40), now),
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
            ResizeFailureRecovery::Superseded {
                by_seq: newer.seq,
            },
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

        let (decision, _) = with_resize_commit_barrier(
            &queue,
            ResizeCancellationToken::new(first.seq),
            || committed.store(true, Ordering::Release),
        );

        assert_eq!(
            decision,
            ResizeCommitDecision::Superseded {
                by_seq: newer.seq,
            },
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
}

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
#[cfg(all(test, feature = "disruptor-pane-io"))]
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
