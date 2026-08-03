//! Runtime authority for capture producer and persistence lifetimes.
//!
//! Numeric pane IDs are reusable.  A queue item identified by only a pane ID
//! can therefore outlive the pane instance that produced it and mutate state
//! belonging to a successor.  This module provides the non-reusable identity
//! and drain protocol used to fence that ABA hazard.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use crate::runtime_async::{notify::Notify, sleep_with_cx, timeout_with_cx};

const LEASE_ADMITTING: u8 = 0;
const LEASE_DRAINING: u8 = 1;
const LEASE_REVOKED: u8 = 2;
const CX_CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Runtime-monotonic identity for one use of a numeric pane ID.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PaneIncarnation(NonZeroU64);

impl PaneIncarnation {
    /// Return the checked monotonic value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// Runtime-monotonic identity for one source activation or reconnect.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SourceEpoch(NonZeroU64);

impl SourceEpoch {
    /// Return the checked monotonic value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// Discovery-owned revision that authorizes one physical capture binding.
///
/// This is separate from [`PaneIncarnation`]: discovery publishes the desired
/// revision before capture activation, while the authority allocates a fresh
/// incarnation only after that exact revision is admitted.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CaptureRevision(NonZeroU64);

impl CaptureRevision {
    /// Construct a checked non-zero discovery revision.
    #[must_use]
    pub const fn new(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Return the revision value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// Runtime-monotonic epoch for one complete discovery authority view.
///
/// Pane revisions identify individual desired bindings.  This epoch orders
/// whole-map installation so a delayed discovery publication cannot regress
/// the authority back to an older set of pane revisions.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CaptureViewEpoch(NonZeroU64);

impl CaptureViewEpoch {
    /// Construct a checked non-zero view epoch.
    #[must_use]
    pub const fn new(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Return the epoch value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// Finite capture-source classification suitable for bounded telemetry.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum CaptureSourceKind {
    /// Snapshot polling through the mux interface.
    Polling,
    /// Vendored direct-mux render-change subscription.
    VendoredStreaming,
    /// Native push-event connection.
    NativePush,
}

/// Stage at which an in-flight authority guard is held.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CaptureGuardStage {
    /// Cursor or bridge mutation through queue admission.
    Producer,
    /// Dequeue through the complete semantic side-effect chain.
    Persistence,
}

/// Immutable identity attached to every capture queue item.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CaptureStamp {
    global_pane_id: u64,
    pane_incarnation: PaneIncarnation,
    source_kind: CaptureSourceKind,
    source_epoch: SourceEpoch,
}

impl CaptureStamp {
    /// Globally routed pane ID duplicated by the captured segment.
    #[must_use]
    pub const fn global_pane_id(self) -> u64 {
        self.global_pane_id
    }

    /// Runtime-monotonic pane incarnation.
    #[must_use]
    pub const fn pane_incarnation(self) -> PaneIncarnation {
        self.pane_incarnation
    }

    /// Bounded producer kind.
    #[must_use]
    pub const fn source_kind(self) -> CaptureSourceKind {
        self.source_kind
    }

    /// Runtime-monotonic producer epoch.
    #[must_use]
    pub const fn source_epoch(self) -> SourceEpoch {
        self.source_epoch
    }
}

/// Pane identity used when issuing or revoking capture sources.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ActivePaneIdentity {
    global_pane_id: u64,
    pane_incarnation: PaneIncarnation,
}

impl ActivePaneIdentity {
    /// Globally routed pane ID.
    #[must_use]
    pub const fn global_pane_id(self) -> u64 {
        self.global_pane_id
    }

    /// Runtime-monotonic pane incarnation.
    #[must_use]
    pub const fn pane_incarnation(self) -> PaneIncarnation {
        self.pane_incarnation
    }
}

/// Finite fail-closed authority errors.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CaptureAuthorityError {
    /// The pane-incarnation namespace cannot advance without wrapping.
    #[error("capture pane-incarnation namespace exhausted")]
    PaneIncarnationExhausted,
    /// The source-epoch namespace cannot advance without wrapping.
    #[error("capture source-epoch namespace exhausted")]
    SourceEpochExhausted,
    /// The numeric pane ID already has an active incarnation.
    #[error("capture pane {global_pane_id} already has an active incarnation")]
    PaneAlreadyActive { global_pane_id: u64 },
    /// Capture activation did not match discovery's currently desired revision.
    #[error("capture pane {global_pane_id} discovery revision is not desired")]
    RevisionNotDesired { global_pane_id: u64 },
    /// Standalone activation was attempted after discovery enabled its gate.
    #[error("capture pane {global_pane_id} requires a discovery revision")]
    RevisionRequired { global_pane_id: u64 },
    /// A delayed or duplicate whole-view installation attempted to regress authority.
    #[error(
        "capture desired view epoch {requested_epoch} is not newer than installed epoch {installed_epoch}"
    )]
    StaleDesiredView {
        installed_epoch: u64,
        requested_epoch: u64,
    },
    /// No incarnation is active for the numeric pane ID.
    #[error("capture pane {global_pane_id} has no active incarnation")]
    PaneNotActive { global_pane_id: u64 },
    /// A caller presented an incarnation that is no longer authoritative.
    #[error("capture pane {global_pane_id} incarnation is stale")]
    StalePaneIncarnation { global_pane_id: u64 },
    /// The exact source kind already has an active or draining lease.
    #[error("capture source {source_kind:?} is already active or draining")]
    SourceAlreadyActive { source_kind: CaptureSourceKind },
    /// No lease exists for the requested source kind.
    #[error("capture source {source_kind:?} is not active")]
    SourceNotActive { source_kind: CaptureSourceKind },
    /// The requested source epoch has already been superseded.
    #[error("capture source {source_kind:?} epoch is stale")]
    StaleSourceEpoch { source_kind: CaptureSourceKind },
    /// A pane or source transition is already draining.
    #[error("capture authority transition is already in progress")]
    TransitionInProgress,
    /// The stamp presented with an event does not belong to this lease.
    #[error("capture event stamp does not match its authority lease")]
    StampMismatch,
    /// The captured segment and stamp disagree about the routed pane.
    #[error("capture event pane ID does not match its authority stamp")]
    PaneIdMismatch,
    /// The selected in-flight counter cannot increment without wrapping.
    #[error("capture {stage:?} in-flight counter exhausted")]
    InflightCounterExhausted { stage: CaptureGuardStage },
    /// Admission raced with revocation and was rejected after increment.
    #[error("capture lease is not admitting new {stage:?} guards")]
    LeaseNotAdmitting { stage: CaptureGuardStage },
    /// The caller cancelled while a revoked lease was draining.
    #[error("capture authority drain was cancelled")]
    DrainCancelled,
    /// The bounded drain deadline expired.  The predecessor remains revoked.
    #[error("capture authority drain timed out")]
    DrainTimedOut,
    /// Authority state changed unexpectedly while completing a drain.
    #[error("capture authority changed during drain completion")]
    AuthorityChanged,
    /// A panic poisoned the safety-critical authority map.
    #[error("capture authority state is poisoned")]
    AuthorityPoisoned,
}

struct CaptureLeaseInner {
    stamp: CaptureStamp,
    state: AtomicU8,
    producer_inflight: AtomicUsize,
    persistence_inflight: AtomicUsize,
    drain_waiter_active: AtomicBool,
    drained: Notify,
    #[cfg(test)]
    drain_notifications: AtomicUsize,
}

/// Cloneable producer handle for one exact capture source epoch.
#[derive(Clone)]
pub struct CaptureLease {
    inner: Arc<CaptureLeaseInner>,
}

impl fmt::Debug for CaptureLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CaptureLease")
            .field("stamp", &self.inner.stamp)
            .field("state", &self.inner.state.load(Ordering::SeqCst))
            .field(
                "producer_inflight",
                &self.inner.producer_inflight.load(Ordering::SeqCst),
            )
            .field(
                "persistence_inflight",
                &self.inner.persistence_inflight.load(Ordering::SeqCst),
            )
            .finish()
    }
}

impl CaptureLease {
    fn new(stamp: CaptureStamp) -> Self {
        Self {
            inner: Arc::new(CaptureLeaseInner {
                stamp,
                state: AtomicU8::new(LEASE_ADMITTING),
                producer_inflight: AtomicUsize::new(0),
                persistence_inflight: AtomicUsize::new(0),
                drain_waiter_active: AtomicBool::new(false),
                drained: Notify::new(),
                #[cfg(test)]
                drain_notifications: AtomicUsize::new(0),
            }),
        }
    }

    /// Exact immutable identity owned by this lease.
    #[must_use]
    pub fn stamp(&self) -> CaptureStamp {
        self.inner.stamp
    }

    /// Acquire producer authority before cursor/bridge mutation.
    pub fn try_acquire_producer(
        &self,
        stamp: CaptureStamp,
        captured_global_pane_id: u64,
    ) -> Result<CaptureProducerGuard, CaptureAuthorityError> {
        self.try_acquire_with_hook(
            stamp,
            captured_global_pane_id,
            CaptureGuardStage::Producer,
            || {},
        )
        .map(CaptureProducerGuard)
    }

    /// Acquire persistence authority immediately after queue dequeue.
    pub fn try_acquire_persistence(
        &self,
        stamp: CaptureStamp,
        captured_global_pane_id: u64,
    ) -> Result<CapturePersistenceGuard, CaptureAuthorityError> {
        self.try_acquire_with_hook(
            stamp,
            captured_global_pane_id,
            CaptureGuardStage::Persistence,
            || {},
        )
        .map(CapturePersistenceGuard)
    }

    fn try_acquire_with_hook<F>(
        &self,
        stamp: CaptureStamp,
        captured_global_pane_id: u64,
        stage: CaptureGuardStage,
        after_increment: F,
    ) -> Result<CaptureInflightGuard, CaptureAuthorityError>
    where
        F: FnOnce(),
    {
        if stamp != self.inner.stamp {
            return Err(CaptureAuthorityError::StampMismatch);
        }
        if captured_global_pane_id != stamp.global_pane_id {
            return Err(CaptureAuthorityError::PaneIdMismatch);
        }
        if self.inner.state.load(Ordering::SeqCst) != LEASE_ADMITTING {
            return Err(CaptureAuthorityError::LeaseNotAdmitting { stage });
        }

        let counter = self.counter(stage);
        try_increment_inflight(counter, stage)?;

        let guard = CaptureInflightGuard {
            inner: Arc::clone(&self.inner),
            stage,
        };

        // Deterministic tests use this seam to revoke between the increment and
        // the mandatory recheck.  The guard already exists so a panicking hook
        // still releases its increment during unwinding.
        after_increment();
        if self.inner.state.load(Ordering::SeqCst) != LEASE_ADMITTING {
            drop(guard);
            return Err(CaptureAuthorityError::LeaseNotAdmitting { stage });
        }

        Ok(guard)
    }

    fn counter(&self, stage: CaptureGuardStage) -> &AtomicUsize {
        match stage {
            CaptureGuardStage::Producer => &self.inner.producer_inflight,
            CaptureGuardStage::Persistence => &self.inner.persistence_inflight,
        }
    }

    fn begin_or_resume_revocation(&self) -> Result<CaptureRevocation, CaptureAuthorityError> {
        let revocation = self.revocation_handle()?;
        self.revoke_admission()?;
        Ok(revocation)
    }

    fn revocation_handle(&self) -> Result<CaptureRevocation, CaptureAuthorityError> {
        match self.inner.state.load(Ordering::SeqCst) {
            LEASE_ADMITTING | LEASE_DRAINING | LEASE_REVOKED => Ok(CaptureRevocation {
                inner: Arc::clone(&self.inner),
            }),
            _ => Err(CaptureAuthorityError::AuthorityChanged),
        }
    }

    fn revoke_admission(&self) -> Result<(), CaptureAuthorityError> {
        loop {
            match self.inner.state.load(Ordering::SeqCst) {
                LEASE_ADMITTING => {
                    if self
                        .inner
                        .state
                        .compare_exchange(
                            LEASE_ADMITTING,
                            LEASE_DRAINING,
                            Ordering::SeqCst,
                            Ordering::SeqCst,
                        )
                        .is_err()
                    {
                        continue;
                    }
                }
                LEASE_DRAINING | LEASE_REVOKED => {}
                _ => return Err(CaptureAuthorityError::AuthorityChanged),
            }
            return Ok(());
        }
    }

    #[cfg(test)]
    fn inflight_counts(&self) -> (usize, usize) {
        (
            self.inner.producer_inflight.load(Ordering::SeqCst),
            self.inner.persistence_inflight.load(Ordering::SeqCst),
        )
    }

    #[cfg(test)]
    fn drain_notification_count(&self) -> usize {
        self.inner.drain_notifications.load(Ordering::SeqCst)
    }
}

fn try_increment_inflight(
    counter: &AtomicUsize,
    stage: CaptureGuardStage,
) -> Result<(), CaptureAuthorityError> {
    let mut current = counter.load(Ordering::SeqCst);
    loop {
        let next = current
            .checked_add(1)
            .ok_or(CaptureAuthorityError::InflightCounterExhausted { stage })?;
        match counter.compare_exchange_weak(
            current,
            next,
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            Ok(_) => return Ok(()),
            Err(actual) => current = actual,
        }
    }
}

fn decrement_inflight_exactly_once(counter: &AtomicUsize) -> usize {
    let mut current = counter.load(Ordering::SeqCst);
    loop {
        let next = current
            .checked_sub(1)
            .ok_or(current)
            .expect("capture in-flight guard released exactly once");
        match counter.compare_exchange_weak(
            current,
            next,
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            Ok(previous) => return previous,
            Err(actual) => current = actual,
        }
    }
}

struct CaptureInflightGuard {
    inner: Arc<CaptureLeaseInner>,
    stage: CaptureGuardStage,
}

impl fmt::Debug for CaptureInflightGuard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CaptureInflightGuard")
            .field("stamp", &self.inner.stamp)
            .field("stage", &self.stage)
            .finish()
    }
}

impl Drop for CaptureInflightGuard {
    fn drop(&mut self) {
        release_inflight(&self.inner, self.stage);
    }
}

fn release_inflight(inner: &CaptureLeaseInner, stage: CaptureGuardStage) {
    let counter = match stage {
        CaptureGuardStage::Producer => &inner.producer_inflight,
        CaptureGuardStage::Persistence => &inner.persistence_inflight,
    };
    let previous = decrement_inflight_exactly_once(counter);
    if previous == 1
        && inner.state.load(Ordering::SeqCst) != LEASE_ADMITTING
        && inner.producer_inflight.load(Ordering::SeqCst) == 0
        && inner.persistence_inflight.load(Ordering::SeqCst) == 0
    {
        #[cfg(test)]
        inner.drain_notifications.fetch_add(1, Ordering::SeqCst);
        inner.drained.notify_one();
    }
}

/// RAII producer admission held through mutation, egress, and enqueue.
#[derive(Debug)]
pub struct CaptureProducerGuard(CaptureInflightGuard);

impl CaptureProducerGuard {
    /// Stamp that must be copied into the resulting queue event.
    #[must_use]
    pub fn stamp(&self) -> CaptureStamp {
        self.0.inner.stamp
    }
}

/// RAII persistence admission held through all semantic side effects.
#[derive(Debug)]
pub struct CapturePersistenceGuard(CaptureInflightGuard);

impl CapturePersistenceGuard {
    /// Stamp authorizing the side-effect chain.
    #[must_use]
    pub fn stamp(&self) -> CaptureStamp {
        self.0.inner.stamp
    }

    /// Mint a non-cloneable child hold for a delegated persistence side effect.
    ///
    /// The caller already holds this admitted parent guard, so delegation is
    /// allowed after revocation has begun: the child is causally part of the
    /// accepted side-effect chain, not a new top-level event.  This is used by
    /// the storage writer handoff, whose queued command can outlive the async
    /// caller that submitted it.  The child keeps pane/source revocation
    /// blocked until that detached command has committed or failed.
    pub(crate) fn delegate_storage(
        &self,
    ) -> Result<CapturePersistenceHold, CaptureAuthorityError> {
        let counter = &self.0.inner.persistence_inflight;
        try_increment_inflight(counter, CaptureGuardStage::Persistence)?;

        Ok(CapturePersistenceHold {
            _guard: CaptureInflightGuard {
                inner: Arc::clone(&self.0.inner),
                stage: CaptureGuardStage::Persistence,
            },
        })
    }
}

/// Non-cloneable, drop-only authority retained by a detached storage command.
///
/// This type deliberately exposes no delegation API: only an admitted parent
/// [`CapturePersistenceGuard`] can mint one direct child for a concrete writer
/// handoff, so queued storage work cannot amplify its own authority.
#[derive(Debug)]
pub(crate) struct CapturePersistenceHold {
    _guard: CaptureInflightGuard,
}

struct CaptureRevocation {
    inner: Arc<CaptureLeaseInner>,
}

impl CaptureRevocation {
    async fn wait_until_drained(&self) -> Result<(), CaptureAuthorityError> {
        let _waiter = self.claim_waiter()?;
        while !self.is_drained() {
            self.inner.drained.notified().await;
        }
        self.finish()?;
        Ok(())
    }

    fn claim_waiter(&self) -> Result<CaptureDrainWaiter, CaptureAuthorityError> {
        self.inner
            .drain_waiter_active
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .map_err(|_| CaptureAuthorityError::TransitionInProgress)?;
        Ok(CaptureDrainWaiter {
            inner: Arc::clone(&self.inner),
        })
    }

    fn is_drained(&self) -> bool {
        self.inner.producer_inflight.load(Ordering::SeqCst) == 0
            && self.inner.persistence_inflight.load(Ordering::SeqCst) == 0
    }

    fn finish(&self) -> Result<(), CaptureAuthorityError> {
        if self.inner.state.load(Ordering::SeqCst) == LEASE_REVOKED {
            return Ok(());
        }
        self.inner
            .state
            .compare_exchange(
                LEASE_DRAINING,
                LEASE_REVOKED,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .map(|_| ())
            .map_err(|_| CaptureAuthorityError::AuthorityChanged)
    }
}

struct CaptureDrainWaiter {
    inner: Arc<CaptureLeaseInner>,
}

impl Drop for CaptureDrainWaiter {
    fn drop(&mut self) {
        self.inner
            .drain_waiter_active
            .store(false, Ordering::SeqCst);
    }
}

async fn await_capture_drain_with_cx<T, F>(
    cx: &crate::cx::Cx,
    timeout: Duration,
    drain: F,
) -> Result<T, CaptureAuthorityError>
where
    F: std::future::Future<Output = Result<T, CaptureAuthorityError>>,
{
    if cx.is_cancel_requested() {
        return Err(CaptureAuthorityError::DrainCancelled);
    }

    use futures::future::{Either, select};

    let bounded_drain = std::pin::pin!(timeout_with_cx(cx, timeout, drain));
    let cancel_watcher = std::pin::pin!(async {
        loop {
            if cx.is_cancel_requested() {
                return;
            }
            let _ = sleep_with_cx(cx, CX_CANCEL_POLL_INTERVAL).await;
        }
    });

    match select(bounded_drain, cancel_watcher).await {
        Either::Left((Ok(result), _)) => result,
        Either::Left((Err(_), _)) if cx.is_cancel_requested() => {
            Err(CaptureAuthorityError::DrainCancelled)
        }
        Either::Left((Err(_), _)) => Err(CaptureAuthorityError::DrainTimedOut),
        Either::Right(((), _)) => Err(CaptureAuthorityError::DrainCancelled),
    }
}

struct PaneAuthorityState {
    identity: ActivePaneIdentity,
    discovery_revision: Option<CaptureRevision>,
    accepting_sources: bool,
    sources: BTreeMap<CaptureSourceKind, CaptureLease>,
}

#[derive(Default)]
struct CaptureAuthorityState {
    last_incarnation: u64,
    last_source_epoch: u64,
    desired_view_epoch: Option<CaptureViewEpoch>,
    desired_revisions: HashMap<u64, CaptureRevision>,
    panes: HashMap<u64, PaneAuthorityState>,
}

struct CaptureAuthorityInner {
    state: Mutex<CaptureAuthorityState>,
}

/// Runtime-owned allocator and transition authority for capture lifetimes.
#[derive(Clone)]
pub struct CaptureAuthority {
    inner: Arc<CaptureAuthorityInner>,
}

impl Default for CaptureAuthority {
    fn default() -> Self {
        Self {
            inner: Arc::new(CaptureAuthorityInner {
                state: Mutex::new(CaptureAuthorityState::default()),
            }),
        }
    }
}

impl CaptureAuthority {
    /// Construct an empty runtime authority.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Install a fresh incarnation for a currently inactive numeric pane ID.
    pub fn activate_pane(
        &self,
        global_pane_id: u64,
    ) -> Result<ActivePaneIdentity, CaptureAuthorityError> {
        self.activate_pane_inner(global_pane_id, None)
    }

    /// Install a fresh pane incarnation only if discovery still desires the
    /// exact revision.  This closes the check-to-exposure race between a watch
    /// publication changing and a provisional successor issuing its sources.
    pub fn activate_pane_for_revision(
        &self,
        global_pane_id: u64,
        revision: CaptureRevision,
    ) -> Result<ActivePaneIdentity, CaptureAuthorityError> {
        self.activate_pane_inner(global_pane_id, Some(revision))
    }

    fn activate_pane_inner(
        &self,
        global_pane_id: u64,
        revision: Option<CaptureRevision>,
    ) -> Result<ActivePaneIdentity, CaptureAuthorityError> {
        let mut state = self.lock_state()?;
        if state.panes.contains_key(&global_pane_id) {
            return Err(CaptureAuthorityError::PaneAlreadyActive { global_pane_id });
        }
        if revision.is_none() && state.desired_view_epoch.is_some() {
            return Err(CaptureAuthorityError::RevisionRequired { global_pane_id });
        }
        if let Some(revision) = revision
            && state.desired_revisions.get(&global_pane_id).copied() != Some(revision)
        {
            return Err(CaptureAuthorityError::RevisionNotDesired { global_pane_id });
        }
        let next = state
            .last_incarnation
            .checked_add(1)
            .ok_or(CaptureAuthorityError::PaneIncarnationExhausted)?;
        let pane_incarnation = PaneIncarnation(
            NonZeroU64::new(next).ok_or(CaptureAuthorityError::PaneIncarnationExhausted)?,
        );
        state.last_incarnation = next;
        let identity = ActivePaneIdentity {
            global_pane_id,
            pane_incarnation,
        };
        state.panes.insert(
            global_pane_id,
            PaneAuthorityState {
                identity,
                discovery_revision: revision,
                accepting_sources: true,
                sources: BTreeMap::new(),
            },
        );
        Ok(identity)
    }

    /// Atomically replace discovery's desired capture-revision view and close
    /// admission for every active gated pane that no longer matches it.
    ///
    /// This method performs no await and revokes lease atomics while holding
    /// the short authority-map mutex.  Therefore either a concurrent gated
    /// activation observes the new desired revision and fails, or it installs
    /// first and is revoked before this method returns.
    pub(crate) fn install_desired_revisions(
        &self,
        epoch: CaptureViewEpoch,
        desired: &HashMap<u64, CaptureRevision>,
    ) -> Result<(), CaptureAuthorityError> {
        let mut state = self.lock_state()?;
        if let Some(installed_epoch) = state.desired_view_epoch
            && epoch <= installed_epoch
        {
            return Err(CaptureAuthorityError::StaleDesiredView {
                installed_epoch: installed_epoch.get(),
                requested_epoch: epoch.get(),
            });
        }
        for (pane_id, pane) in &mut state.panes {
            if pane
                .discovery_revision
                .is_some_and(|active_revision| {
                    desired.get(pane_id).copied() == Some(active_revision)
                })
            {
                continue;
            }
            pane.accepting_sources = false;
            for lease in pane.sources.values() {
                lease.revoke_admission()?;
            }
        }
        state.desired_view_epoch = Some(epoch);
        state.desired_revisions.clone_from(desired);
        Ok(())
    }

    /// Return every numeric pane ID whose exact incarnation still exists.
    ///
    /// Non-admitting/draining panes are intentionally included: registry
    /// absence alone is not proof that their admitted producer or persistence
    /// work has completed, so lifecycle GC must retain their cursor and
    /// semantic state until exact retirement removes the authority entry.
    pub fn retained_pane_ids(&self) -> Result<HashSet<u64>, CaptureAuthorityError> {
        let state = self.lock_state()?;
        Ok(state.panes.keys().copied().collect())
    }

    /// Issue one source epoch for an active pane incarnation.
    pub fn issue_source(
        &self,
        identity: ActivePaneIdentity,
        source_kind: CaptureSourceKind,
    ) -> Result<CaptureLease, CaptureAuthorityError> {
        let mut state = self.lock_state()?;
        let pane = state
            .panes
            .get(&identity.global_pane_id)
            .ok_or(CaptureAuthorityError::PaneNotActive {
                global_pane_id: identity.global_pane_id,
            })?;
        if pane.identity != identity {
            return Err(CaptureAuthorityError::StalePaneIncarnation {
                global_pane_id: identity.global_pane_id,
            });
        }
        if !pane.accepting_sources {
            return Err(CaptureAuthorityError::TransitionInProgress);
        }
        if pane.sources.contains_key(&source_kind) {
            return Err(CaptureAuthorityError::SourceAlreadyActive { source_kind });
        }

        let next = state
            .last_source_epoch
            .checked_add(1)
            .ok_or(CaptureAuthorityError::SourceEpochExhausted)?;
        let source_epoch = SourceEpoch(
            NonZeroU64::new(next).ok_or(CaptureAuthorityError::SourceEpochExhausted)?,
        );
        state.last_source_epoch = next;
        let lease = CaptureLease::new(CaptureStamp {
            global_pane_id: identity.global_pane_id,
            pane_incarnation: identity.pane_incarnation,
            source_kind,
            source_epoch,
        });
        state
            .panes
            .get_mut(&identity.global_pane_id)
            .ok_or(CaptureAuthorityError::AuthorityChanged)?
            .sources
            .insert(source_kind, lease.clone());
        Ok(lease)
    }

    /// Acquire persistence authority for an exact queued-event stamp.
    ///
    /// The lease is cloned while the authority map is locked, then admission
    /// performs its increment-and-recheck after releasing that lock.  A
    /// concurrent revocation therefore either observes this guard in its
    /// drain count or makes the admission fail closed.  Queued events whose
    /// pane/source entry has already been retired cannot accidentally bind to
    /// a same-ID successor because both incarnation and source epoch are
    /// compared exactly.
    pub fn try_acquire_persistence(
        &self,
        stamp: CaptureStamp,
        captured_global_pane_id: u64,
    ) -> Result<CapturePersistenceGuard, CaptureAuthorityError> {
        if captured_global_pane_id != stamp.global_pane_id() {
            return Err(CaptureAuthorityError::PaneIdMismatch);
        }

        let lease = {
            let state = self.lock_state()?;
            let pane = state
                .panes
                .get(&stamp.global_pane_id())
                .ok_or_else(|| CaptureAuthorityError::PaneNotActive {
                    global_pane_id: stamp.global_pane_id(),
                })?;
            if pane.identity.pane_incarnation() != stamp.pane_incarnation() {
                return Err(CaptureAuthorityError::StalePaneIncarnation {
                    global_pane_id: stamp.global_pane_id(),
                });
            }
            let source_kind = stamp.source_kind();
            let lease = pane
                .sources
                .get(&source_kind)
                .ok_or(CaptureAuthorityError::SourceNotActive { source_kind })?;
            if lease.stamp() != stamp {
                return Err(CaptureAuthorityError::StaleSourceEpoch { source_kind });
            }
            lease.clone()
        };

        lease.try_acquire_persistence(stamp, captured_global_pane_id)
    }

    /// Provisional native-ingress lookup for frames whose wire envelope does
    /// not yet carry its connection/source epoch.
    ///
    /// This is intentionally crate-private and hard-coded to `NativePush` so
    /// polling and streaming producers cannot accidentally relabel stale work.
    /// The native connection-envelope child must replace this lookup with exact
    /// stamp admission; until then, holding the returned guard before
    /// coalescing at least prevents authority replacement after ingress.
    #[cfg(feature = "native-wezterm")]
    pub(crate) fn try_acquire_unstamped_native_producer(
        &self,
        global_pane_id: u64,
    ) -> Result<CaptureProducerGuard, CaptureAuthorityError> {
        let source_kind = CaptureSourceKind::NativePush;
        let lease = {
            let state = self.lock_state()?;
            let pane = state
                .panes
                .get(&global_pane_id)
                .ok_or(CaptureAuthorityError::PaneNotActive { global_pane_id })?;
            pane.sources
                .get(&source_kind)
                .cloned()
                .ok_or(CaptureAuthorityError::SourceNotActive { source_kind })?
        };
        lease.try_acquire_producer(lease.stamp(), global_pane_id)
    }

    /// Begin or resume revoking one source.  A replacement handle can be
    /// reacquired after timeout, cancellation, or task loss.
    pub fn begin_source_revocation(
        &self,
        identity: ActivePaneIdentity,
        expected_stamp: CaptureStamp,
    ) -> Result<SourceRevocation, CaptureAuthorityError> {
        let state = self.lock_state()?;
        let pane = Self::authoritative_pane(&state, identity)?;
        let source_kind = expected_stamp.source_kind();
        let lease = pane
            .sources
            .get(&source_kind)
            .ok_or(CaptureAuthorityError::SourceNotActive { source_kind })?;
        if lease.stamp() != expected_stamp {
            return Err(CaptureAuthorityError::StaleSourceEpoch { source_kind });
        }
        let revocation = lease.begin_or_resume_revocation()?;
        Ok(SourceRevocation {
            authority: self.clone(),
            identity,
            source_kind,
            stamp: expected_stamp,
            revocation,
        })
    }

    /// Begin retiring an entire pane incarnation.  New sources are refused
    /// before any lease starts draining.
    pub fn begin_pane_revocation(
        &self,
        identity: ActivePaneIdentity,
    ) -> Result<PaneRevocation, CaptureAuthorityError> {
        let mut state = self.lock_state()?;
        let pane = Self::authoritative_pane_mut(&mut state, identity)?;
        let leases = pane.sources.values().cloned().collect::<Vec<_>>();
        let revocations = leases
            .iter()
            .map(CaptureLease::revocation_handle)
            .collect::<Result<Vec<_>, _>>()?;
        pane.accepting_sources = false;
        for lease in &leases {
            lease.revoke_admission()?;
        }
        Ok(PaneRevocation {
            authority: self.clone(),
            identity,
            revocations,
        })
    }

    /// Revoke a pane synchronously and retire it only when no admitted work
    /// remains.
    ///
    /// Standalone polling owns its task set outside the supervisor, so waiting
    /// here would deadlock the very futures that must release their producer
    /// guards. Returning `Ok(false)` leaves the exact predecessor
    /// non-admitting and lets the caller poll those tasks before retrying.
    pub(crate) fn retire_pane_if_drained(
        &self,
        identity: ActivePaneIdentity,
    ) -> Result<bool, CaptureAuthorityError> {
        let mut state = self.lock_state()?;
        let pane = Self::authoritative_pane_mut(&mut state, identity)?;
        let leases = pane.sources.values().cloned().collect::<Vec<_>>();
        let revocations = leases
            .iter()
            .map(CaptureLease::revocation_handle)
            .collect::<Result<Vec<_>, _>>()?;

        pane.accepting_sources = false;
        for lease in &leases {
            lease.revoke_admission()?;
        }
        if revocations
            .iter()
            .any(|revocation| !revocation.is_drained())
        {
            return Ok(false);
        }
        for revocation in &revocations {
            revocation.finish()?;
        }
        state.panes.remove(&identity.global_pane_id);
        Ok(true)
    }

    fn authoritative_pane(
        state: &CaptureAuthorityState,
        identity: ActivePaneIdentity,
    ) -> Result<&PaneAuthorityState, CaptureAuthorityError> {
        let pane = state
            .panes
            .get(&identity.global_pane_id)
            .ok_or(CaptureAuthorityError::PaneNotActive {
                global_pane_id: identity.global_pane_id,
            })?;
        if pane.identity != identity {
            return Err(CaptureAuthorityError::StalePaneIncarnation {
                global_pane_id: identity.global_pane_id,
            });
        }
        Ok(pane)
    }

    fn authoritative_pane_mut(
        state: &mut CaptureAuthorityState,
        identity: ActivePaneIdentity,
    ) -> Result<&mut PaneAuthorityState, CaptureAuthorityError> {
        let pane = state
            .panes
            .get_mut(&identity.global_pane_id)
            .ok_or(CaptureAuthorityError::PaneNotActive {
                global_pane_id: identity.global_pane_id,
            })?;
        if pane.identity != identity {
            return Err(CaptureAuthorityError::StalePaneIncarnation {
                global_pane_id: identity.global_pane_id,
            });
        }
        Ok(pane)
    }

    fn lock_state(
        &self,
    ) -> Result<MutexGuard<'_, CaptureAuthorityState>, CaptureAuthorityError> {
        self.inner
            .state
            .lock()
            .map_err(|_| CaptureAuthorityError::AuthorityPoisoned)
    }
}

/// Retryable drain handle for one source epoch.
#[must_use = "dropping this handle leaves the source non-admitting; begin_source_revocation resumes it"]
pub struct SourceRevocation {
    authority: CaptureAuthority,
    identity: ActivePaneIdentity,
    source_kind: CaptureSourceKind,
    stamp: CaptureStamp,
    revocation: CaptureRevocation,
}

impl SourceRevocation {
    /// Revoke admission, await both in-flight stages, then remove this exact
    /// source epoch from the authority map.  No authority lock crosses await.
    pub async fn wait_with_cx(
        &self,
        cx: &crate::cx::Cx,
        timeout: Duration,
    ) -> Result<CaptureStamp, CaptureAuthorityError> {
        await_capture_drain_with_cx(cx, timeout, self.revocation.wait_until_drained()).await?;
        let mut state = self.authority.lock_state()?;
        let pane = CaptureAuthority::authoritative_pane_mut(&mut state, self.identity)?;
        if pane
            .sources
            .get(&self.source_kind)
            .is_none_or(|lease| lease.stamp() != self.stamp)
        {
            return Err(CaptureAuthorityError::AuthorityChanged);
        }
        pane.sources.remove(&self.source_kind);
        Ok(self.stamp)
    }
}

/// Retryable drain handle for one complete pane incarnation.
#[must_use = "dropping this handle leaves the pane non-admitting; begin_pane_revocation resumes it"]
pub struct PaneRevocation {
    authority: CaptureAuthority,
    identity: ActivePaneIdentity,
    revocations: Vec<CaptureRevocation>,
}

impl PaneRevocation {
    /// Drain all source epochs under one total deadline, then retire the pane.
    /// On timeout the predecessor remains non-admitting.  Retrying this handle
    /// or reacquiring one from the authority resumes the exact transition.
    pub async fn wait_with_cx(
        &self,
        cx: &crate::cx::Cx,
        timeout: Duration,
    ) -> Result<ActivePaneIdentity, CaptureAuthorityError> {
        let drain_all = async {
            for revocation in &self.revocations {
                revocation.wait_until_drained().await?;
            }
            Ok(())
        };
        await_capture_drain_with_cx(cx, timeout, drain_all).await?;

        let mut state = self.authority.lock_state()?;
        let pane = CaptureAuthority::authoritative_pane(&state, self.identity)?;
        if pane.accepting_sources
            || pane
                .sources
                .values()
                .any(|lease| lease.inner.state.load(Ordering::SeqCst) != LEASE_REVOKED)
        {
            return Err(CaptureAuthorityError::AuthorityChanged);
        }
        state.panes.remove(&self.identity.global_pane_id);
        Ok(self.identity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_async::CompatRuntime;

    fn run_async_test<F>(future: F)
    where
        F: std::future::Future<Output = ()>,
    {
        crate::runtime_async::RuntimeBuilder::current_thread()
            .build()
            .expect("capture authority test runtime")
            .block_on(future);
    }

    #[test]
    fn runtime_state_retention_includes_draining_incarnations_until_exact_retirement() {
        let authority = CaptureAuthority::new();
        let identity = authority.activate_pane(7).expect("activate pane");
        let lease = authority
            .issue_source(identity, CaptureSourceKind::Polling)
            .expect("polling source");
        let held = lease
            .try_acquire_persistence(lease.stamp(), 7)
            .expect("held persistence guard");

        let _revocation = authority
            .begin_pane_revocation(identity)
            .expect("begin exact pane revocation");
        assert_eq!(
            authority.retained_pane_ids().expect("retention snapshot"),
            HashSet::from([7]),
            "registry GC must retain non-admitting panes while exact work drains"
        );
        assert!(
            !authority
                .retire_pane_if_drained(identity)
                .expect("retry held retirement")
        );

        drop(held);
        assert!(
            authority
                .retire_pane_if_drained(identity)
                .expect("retire drained pane")
        );
        assert!(
            authority
                .retained_pane_ids()
                .expect("post-retirement snapshot")
                .is_empty()
        );
    }

    #[test]
    fn desired_revision_gate_closes_activation_and_source_exposure_races() {
        let authority = CaptureAuthority::new();
        let revision_one = CaptureRevision::new(1).expect("revision one");
        let revision_two = CaptureRevision::new(2).expect("revision two");
        let revision_three = CaptureRevision::new(3).expect("revision three");
        let desired_one = HashMap::from([(7, revision_one), (8, revision_one)]);
        authority
            .install_desired_revisions(
                CaptureViewEpoch::new(1).expect("view one"),
                &desired_one,
            )
            .expect("install first desired view");
        let predecessor_seven = authority
            .activate_pane_for_revision(7, revision_one)
            .expect("activate pane seven predecessor");
        let predecessor_eight = authority
            .activate_pane_for_revision(8, revision_one)
            .expect("activate pane eight predecessor");
        let lease_seven = authority
            .issue_source(predecessor_seven, CaptureSourceKind::Polling)
            .expect("pane seven predecessor source");
        let lease_eight = authority
            .issue_source(predecessor_eight, CaptureSourceKind::Polling)
            .expect("pane eight predecessor source");

        let desired_two = HashMap::from([(7, revision_two), (8, revision_one)]);
        let producer_race = lease_seven.try_acquire_with_hook(
            lease_seven.stamp(),
            7,
            CaptureGuardStage::Producer,
            || {
                authority
                    .install_desired_revisions(
                        CaptureViewEpoch::new(2).expect("view two"),
                        &desired_two,
                    )
                    .expect("supersede after producer increment");
            },
        );
        assert_eq!(
            producer_race.unwrap_err(),
            CaptureAuthorityError::LeaseNotAdmitting {
                stage: CaptureGuardStage::Producer,
            }
        );
        assert_eq!(lease_seven.inflight_counts(), (0, 0));

        let desired_three = HashMap::from([(7, revision_two), (8, revision_two)]);
        let persistence_race = lease_eight.try_acquire_with_hook(
            lease_eight.stamp(),
            8,
            CaptureGuardStage::Persistence,
            || {
                authority
                    .install_desired_revisions(
                        CaptureViewEpoch::new(3).expect("view three"),
                        &desired_three,
                    )
                    .expect("supersede after persistence increment");
            },
        );
        assert_eq!(
            persistence_race.unwrap_err(),
            CaptureAuthorityError::LeaseNotAdmitting {
                stage: CaptureGuardStage::Persistence,
            }
        );
        assert_eq!(lease_eight.inflight_counts(), (0, 0));

        assert!(
            authority
                .retire_pane_if_drained(predecessor_seven)
                .expect("retire pane seven predecessor")
        );
        assert!(
            authority
                .retire_pane_if_drained(predecessor_eight)
                .expect("retire pane eight predecessor")
        );
        assert_eq!(
            authority
                .activate_pane_for_revision(7, revision_one)
                .unwrap_err(),
            CaptureAuthorityError::RevisionNotDesired { global_pane_id: 7 }
        );
        let provisional_successor = authority
            .activate_pane_for_revision(7, revision_two)
            .expect("activate desired successor");

        let desired_four = HashMap::from([(7, revision_three), (8, revision_two)]);
        authority
            .install_desired_revisions(
                CaptureViewEpoch::new(4).expect("view four"),
                &desired_four,
            )
            .expect("supersede between activation and source exposure");
        assert_eq!(
            authority
                .issue_source(provisional_successor, CaptureSourceKind::Polling)
                .unwrap_err(),
            CaptureAuthorityError::TransitionInProgress
        );
        assert!(
            authority
                .retire_pane_if_drained(provisional_successor)
                .expect("retire unexposed provisional successor")
        );

        let desired_five = HashMap::from([(9, revision_three)]);
        authority
            .install_desired_revisions(
                CaptureViewEpoch::new(5).expect("view five"),
                &desired_five,
            )
            .expect("install pane nine desired view");
        let desired_six = HashMap::from([(9, revision_two)]);
        authority
            .install_desired_revisions(
                CaptureViewEpoch::new(6).expect("view six"),
                &desired_six,
            )
            .expect("supersede pane nine before activation");
        assert_eq!(
            authority
                .activate_pane_for_revision(9, revision_three)
                .unwrap_err(),
            CaptureAuthorityError::RevisionNotDesired { global_pane_id: 9 }
        );
        assert_eq!(
            authority
                .install_desired_revisions(
                    CaptureViewEpoch::new(5).expect("stale view five"),
                    &desired_five,
                )
                .unwrap_err(),
            CaptureAuthorityError::StaleDesiredView {
                installed_epoch: 6,
                requested_epoch: 5,
            }
        );
    }

    #[test]
    fn enabling_discovery_gate_revokes_legacy_authority_and_forbids_bypass() {
        let authority = CaptureAuthority::new();
        let legacy = authority.activate_pane(40).expect("legacy pane");
        let legacy_lease = authority
            .issue_source(legacy, CaptureSourceKind::Polling)
            .expect("legacy source");
        let held = legacy_lease
            .try_acquire_persistence(legacy_lease.stamp(), 40)
            .expect("admitted legacy persistence guard");
        let revision = CaptureRevision::new(1).expect("capture revision");
        let desired = HashMap::from([(41, revision)]);

        authority
            .install_desired_revisions(
                CaptureViewEpoch::new(1).expect("first gated view"),
                &desired,
            )
            .expect("enable discovery gate");
        assert_eq!(
            legacy_lease
                .try_acquire_producer(legacy_lease.stamp(), 40)
                .unwrap_err(),
            CaptureAuthorityError::LeaseNotAdmitting {
                stage: CaptureGuardStage::Producer,
            }
        );
        assert!(
            !authority
                .retire_pane_if_drained(legacy)
                .expect("held legacy guard keeps exact drain open")
        );
        drop(held);
        assert!(
            authority
                .retire_pane_if_drained(legacy)
                .expect("legacy pane drains after admitted work finishes")
        );
        assert_eq!(
            authority.activate_pane(42).unwrap_err(),
            CaptureAuthorityError::RevisionRequired { global_pane_id: 42 }
        );

        let gated = authority
            .activate_pane_for_revision(41, revision)
            .expect("desired gated pane");
        let gated_lease = authority
            .issue_source(gated, CaptureSourceKind::Polling)
            .expect("desired gated source");
        authority
            .install_desired_revisions(
                CaptureViewEpoch::new(2).expect("empty gated view"),
                &HashMap::new(),
            )
            .expect("remove desired pane");
        assert_eq!(
            gated_lease
                .try_acquire_persistence(gated_lease.stamp(), 41)
                .unwrap_err(),
            CaptureAuthorityError::LeaseNotAdmitting {
                stage: CaptureGuardStage::Persistence,
            }
        );
    }

    #[test]
    fn pane_incarnations_and_source_epochs_never_reuse_after_drain() {
        run_async_test(async {
            let authority = CaptureAuthority::new();
            let first = authority.activate_pane(7).expect("first incarnation");
            let first_lease = authority
                .issue_source(first, CaptureSourceKind::Polling)
                .expect("first source");
            let source_revocation = authority
                .begin_source_revocation(first, first_lease.stamp())
                .expect("begin source revocation");
            source_revocation
                .wait_with_cx(
                    &crate::cx::for_testing(),
                    Duration::from_millis(50),
                )
                .await
                .expect("drain first source");
            let second_lease = authority
                .issue_source(first, CaptureSourceKind::Polling)
                .expect("replacement source");
            assert!(
                second_lease.stamp().source_epoch().get()
                    > first_lease.stamp().source_epoch().get()
            );

            let pane_revocation = authority
                .begin_pane_revocation(first)
                .expect("begin pane revocation");
            pane_revocation
                .wait_with_cx(
                    &crate::cx::for_testing(),
                    Duration::from_millis(50),
                )
                .await
                .expect("drain first pane");
            let second = authority.activate_pane(7).expect("second incarnation");
            assert!(second.pane_incarnation().get() > first.pane_incarnation().get());
        });
    }

    #[test]
    fn synchronous_retirement_stays_closed_until_admitted_work_drains() {
        let authority = CaptureAuthority::new();
        let predecessor_pane = authority.activate_pane(8).expect("predecessor pane");
        let predecessor = authority
            .issue_source(predecessor_pane, CaptureSourceKind::Polling)
            .expect("predecessor source");
        let predecessor_stamp = predecessor.stamp();
        let held = predecessor
            .try_acquire_producer(predecessor_stamp, 8)
            .expect("held predecessor producer");

        assert!(!authority
            .retire_pane_if_drained(predecessor_pane)
            .expect("begin synchronous retirement"));
        assert_eq!(
            predecessor
                .try_acquire_producer(predecessor_stamp, 8)
                .unwrap_err(),
            CaptureAuthorityError::LeaseNotAdmitting {
                stage: CaptureGuardStage::Producer,
            }
        );

        drop(held);
        assert!(authority
            .retire_pane_if_drained(predecessor_pane)
            .expect("finish synchronous retirement"));
        let successor_pane = authority.activate_pane(8).expect("successor pane");
        let successor = authority
            .issue_source(successor_pane, CaptureSourceKind::Polling)
            .expect("successor source");
        assert_ne!(successor.stamp(), predecessor_stamp);
        assert_eq!(
            authority
                .try_acquire_persistence(predecessor_stamp, 8)
                .unwrap_err(),
            CaptureAuthorityError::StalePaneIncarnation { global_pane_id: 8 }
        );
    }

    #[test]
    fn revocation_rejects_new_guards_and_is_retryable_after_timeout() {
        run_async_test(async {
            let authority = CaptureAuthority::new();
            let pane = authority.activate_pane(9).expect("pane");
            let lease = authority
                .issue_source(pane, CaptureSourceKind::VendoredStreaming)
                .expect("source");
            let stamp = lease.stamp();
            let producer = lease
                .try_acquire_producer(stamp, stamp.global_pane_id())
                .expect("producer guard");
            let revocation = authority
                .begin_source_revocation(pane, stamp)
                .expect("begin revocation");
            assert_eq!(
                lease
                    .try_acquire_persistence(stamp, stamp.global_pane_id())
                    .unwrap_err(),
                CaptureAuthorityError::LeaseNotAdmitting {
                    stage: CaptureGuardStage::Persistence,
                }
            );
            assert_eq!(
                revocation
                    .wait_with_cx(&crate::cx::for_testing(), Duration::ZERO)
                    .await
                    .unwrap_err(),
                CaptureAuthorityError::DrainTimedOut
            );
            drop(producer);
            assert_eq!(lease.inflight_counts(), (0, 0));
            assert_eq!(
                revocation
                    .wait_with_cx(
                        &crate::cx::for_testing(),
                        Duration::from_millis(50),
                    )
                    .await
                    .expect("retry drain"),
                stamp
            );
        });
    }

    #[test]
    fn ordinary_guard_drops_do_not_accumulate_drain_notifications() {
        run_async_test(async {
            let authority = CaptureAuthority::new();
            let pane = authority.activate_pane(10).expect("pane");
            let lease = authority
                .issue_source(pane, CaptureSourceKind::Polling)
                .expect("source");
            let stamp = lease.stamp();
            for _ in 0..10_000 {
                drop(
                    lease
                        .try_acquire_producer(stamp, stamp.global_pane_id())
                        .expect("ordinary producer guard"),
                );
            }
            assert_eq!(lease.drain_notification_count(), 0);

            let held = lease
                .try_acquire_producer(stamp, stamp.global_pane_id())
                .expect("held producer guard");
            let revocation = authority
                .begin_source_revocation(pane, stamp)
                .expect("begin revocation");
            assert_eq!(lease.drain_notification_count(), 0);
            drop(held);
            assert_eq!(lease.drain_notification_count(), 1);
            revocation
                .wait_with_cx(
                    &crate::cx::for_testing(),
                    Duration::from_millis(50),
                )
                .await
                .expect("drain after final guard");
        });
    }

    #[test]
    fn increment_then_recheck_releases_a_guard_that_races_revocation() {
        let authority = CaptureAuthority::new();
        let pane = authority.activate_pane(11).expect("pane");
        let lease = authority
            .issue_source(pane, CaptureSourceKind::Polling)
            .expect("source");
        let result = lease.try_acquire_with_hook(
            lease.stamp(),
            lease.stamp().global_pane_id(),
            CaptureGuardStage::Producer,
            || {
                lease.inner.state.store(LEASE_DRAINING, Ordering::SeqCst);
            },
        );
        assert_eq!(
            result.unwrap_err(),
            CaptureAuthorityError::LeaseNotAdmitting {
                stage: CaptureGuardStage::Producer,
            }
        );
        assert_eq!(lease.inflight_counts(), (0, 0));
    }

    #[test]
    fn panicking_admission_hook_cannot_leak_its_increment() {
        let authority = CaptureAuthority::new();
        let pane = authority.activate_pane(12).expect("pane");
        let lease = authority
            .issue_source(pane, CaptureSourceKind::Polling)
            .expect("source");
        let stamp = lease.stamp();
        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = lease.try_acquire_with_hook(
                stamp,
                stamp.global_pane_id(),
                CaptureGuardStage::Producer,
                || panic!("exercise admission-hook unwind"),
            );
        }));
        assert!(unwind.is_err());
        assert_eq!(lease.inflight_counts(), (0, 0));
    }

    #[test]
    fn guards_release_during_unwind() {
        let authority = CaptureAuthority::new();
        let pane = authority.activate_pane(13).expect("pane");
        let lease = authority
            .issue_source(pane, CaptureSourceKind::NativePush)
            .expect("source");
        let stamp = lease.stamp();
        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = lease
                .try_acquire_persistence(stamp, stamp.global_pane_id())
                .expect("persistence guard");
            panic!("exercise capture guard unwind");
        }));
        assert!(unwind.is_err());
        assert_eq!(lease.inflight_counts(), (0, 0));
    }

    #[test]
    fn a_lease_rejects_another_sources_stamp() {
        let authority = CaptureAuthority::new();
        let pane = authority.activate_pane(15).expect("pane");
        let polling = authority
            .issue_source(pane, CaptureSourceKind::Polling)
            .expect("polling source");
        let streaming = authority
            .issue_source(pane, CaptureSourceKind::VendoredStreaming)
            .expect("streaming source");
        assert_eq!(
            polling
                .try_acquire_persistence(
                    streaming.stamp(),
                    streaming.stamp().global_pane_id(),
                )
                .unwrap_err(),
            CaptureAuthorityError::StampMismatch
        );
    }

    #[test]
    fn delayed_old_source_exit_cannot_revoke_its_successor_epoch() {
        run_async_test(async {
            let authority = CaptureAuthority::new();
            let pane = authority.activate_pane(151).expect("pane");
            let predecessor = authority
                .issue_source(pane, CaptureSourceKind::Polling)
                .expect("predecessor source");
            let stale_stamp = predecessor.stamp();
            authority
                .begin_source_revocation(pane, stale_stamp)
                .expect("predecessor revocation")
                .wait_with_cx(
                    &crate::cx::for_testing(),
                    Duration::from_millis(50),
                )
                .await
                .expect("predecessor drain");

            let successor = authority
                .issue_source(pane, CaptureSourceKind::Polling)
                .expect("successor source");
            let stale_error = match authority.begin_source_revocation(pane, stale_stamp) {
                Ok(_) => panic!("stale revocation unexpectedly succeeded"),
                Err(error) => error,
            };
            assert_eq!(
                stale_error,
                CaptureAuthorityError::StaleSourceEpoch {
                    source_kind: CaptureSourceKind::Polling,
                }
            );
            let successor_stamp = successor.stamp();
            drop(
                successor
                    .try_acquire_producer(
                        successor_stamp,
                        successor_stamp.global_pane_id(),
                    )
                    .expect("successor remains admitting"),
            );
        });
    }

    #[test]
    fn captured_pane_id_mismatch_is_rejected_before_counter_admission() {
        let authority = CaptureAuthority::new();
        let pane = authority.activate_pane(16).expect("pane");
        let lease = authority
            .issue_source(pane, CaptureSourceKind::Polling)
            .expect("source");
        let stamp = lease.stamp();
        assert_eq!(
            lease
                .try_acquire_persistence(stamp, stamp.global_pane_id() + 1)
                .unwrap_err(),
            CaptureAuthorityError::PaneIdMismatch
        );
        assert_eq!(lease.inflight_counts(), (0, 0));
    }

    #[test]
    fn authority_lookup_rejects_replaced_source_and_reused_pane_id() {
        run_async_test(async {
            let authority = CaptureAuthority::new();
            let predecessor_pane = authority.activate_pane(17).expect("predecessor pane");
            let predecessor = authority
                .issue_source(predecessor_pane, CaptureSourceKind::Polling)
                .expect("predecessor source");
            let predecessor_stamp = predecessor.stamp();

            drop(
                authority
                    .try_acquire_persistence(predecessor_stamp, 17)
                    .expect("current exact lookup"),
            );
            authority
                .begin_source_revocation(predecessor_pane, predecessor_stamp)
                .expect("predecessor source revocation")
                .wait_with_cx(
                    &crate::cx::for_testing(),
                    Duration::from_millis(50),
                )
                .await
                .expect("predecessor source drain");
            let successor_source = authority
                .issue_source(predecessor_pane, CaptureSourceKind::Polling)
                .expect("successor source");
            assert_eq!(
                authority
                    .try_acquire_persistence(predecessor_stamp, 17)
                    .unwrap_err(),
                CaptureAuthorityError::StaleSourceEpoch {
                    source_kind: CaptureSourceKind::Polling,
                }
            );

            authority
                .begin_pane_revocation(predecessor_pane)
                .expect("predecessor pane revocation")
                .wait_with_cx(
                    &crate::cx::for_testing(),
                    Duration::from_millis(50),
                )
                .await
                .expect("predecessor pane drain");
            drop(successor_source);
            let successor_pane = authority.activate_pane(17).expect("successor pane");
            let _successor_lease = authority
                .issue_source(successor_pane, CaptureSourceKind::Polling)
                .expect("successor pane source");
            assert_eq!(
                authority
                    .try_acquire_persistence(predecessor_stamp, 17)
                    .unwrap_err(),
                CaptureAuthorityError::StalePaneIncarnation { global_pane_id: 17 }
            );
        });
    }

    #[test]
    fn delegated_storage_hold_blocks_drain_after_parent_drop() {
        run_async_test(async {
            let authority = CaptureAuthority::new();
            let pane = authority.activate_pane(29).expect("pane");
            let lease = authority
                .issue_source(pane, CaptureSourceKind::Polling)
                .expect("source");
            let stamp = lease.stamp();
            let parent = authority
                .try_acquire_persistence(stamp, 29)
                .expect("parent persistence guard");
            let storage_hold = parent.delegate_storage().expect("delegated storage hold");
            drop(parent);

            let revocation = authority
                .begin_source_revocation(pane, stamp)
                .expect("source revocation");
            assert_eq!(
                revocation
                    .wait_with_cx(
                        &crate::cx::for_testing(),
                        Duration::from_millis(1),
                    )
                    .await
                    .unwrap_err(),
                CaptureAuthorityError::DrainTimedOut
            );
            drop(storage_hold);
            assert_eq!(
                revocation
                    .wait_with_cx(
                        &crate::cx::for_testing(),
                        Duration::from_millis(50),
                    )
                    .await
                    .expect("drain after storage completion"),
                stamp
            );
        });
    }

    #[test]
    fn delegated_storage_hold_counter_exhaustion_fails_closed() {
        let authority = CaptureAuthority::new();
        let pane = authority.activate_pane(30).expect("pane");
        let lease = authority
            .issue_source(pane, CaptureSourceKind::Polling)
            .expect("source");
        let stamp = lease.stamp();
        let parent = authority
            .try_acquire_persistence(stamp, 30)
            .expect("parent persistence guard");
        lease
            .inner
            .persistence_inflight
            .store(usize::MAX, Ordering::SeqCst);
        assert_eq!(
            parent.delegate_storage().unwrap_err(),
            CaptureAuthorityError::InflightCounterExhausted {
                stage: CaptureGuardStage::Persistence,
            }
        );
        lease
            .inner
            .persistence_inflight
            .store(1, Ordering::SeqCst);
        drop(parent);
        assert_eq!(lease.inflight_counts(), (0, 0));
    }

    #[test]
    fn dropped_source_revocation_handle_is_reacquired_without_reopening_admission() {
        run_async_test(async {
            let authority = CaptureAuthority::new();
            let pane = authority.activate_pane(18).expect("pane");
            let lease = authority
                .issue_source(pane, CaptureSourceKind::Polling)
                .expect("source");
            let stamp = lease.stamp();
            let held = lease
                .try_acquire_producer(stamp, stamp.global_pane_id())
                .expect("held producer");
            drop(
                authority
                    .begin_source_revocation(pane, stamp)
                    .expect("first revocation handle"),
            );
            assert_eq!(
                lease
                    .try_acquire_producer(stamp, stamp.global_pane_id())
                    .unwrap_err(),
                CaptureAuthorityError::LeaseNotAdmitting {
                    stage: CaptureGuardStage::Producer,
                }
            );
            let resumed = authority
                .begin_source_revocation(pane, stamp)
                .expect("resumed revocation handle");
            drop(held);
            assert_eq!(
                resumed
                    .wait_with_cx(
                        &crate::cx::for_testing(),
                        Duration::from_millis(50),
                    )
                    .await
                    .expect("resumed drain"),
                stamp
            );
        });
    }

    #[test]
    fn dropped_pane_revocation_handle_is_reacquired_without_reopening_sources() {
        run_async_test(async {
            let authority = CaptureAuthority::new();
            let pane = authority.activate_pane(19).expect("pane");
            let lease = authority
                .issue_source(pane, CaptureSourceKind::NativePush)
                .expect("source");
            let stamp = lease.stamp();
            let held = lease
                .try_acquire_persistence(stamp, stamp.global_pane_id())
                .expect("held persistence");
            drop(
                authority
                    .begin_pane_revocation(pane)
                    .expect("first pane revocation handle"),
            );
            assert_eq!(
                authority
                    .issue_source(pane, CaptureSourceKind::Polling)
                    .unwrap_err(),
                CaptureAuthorityError::TransitionInProgress
            );
            let resumed = authority
                .begin_pane_revocation(pane)
                .expect("resumed pane revocation handle");
            drop(held);
            assert_eq!(
                resumed
                    .wait_with_cx(
                        &crate::cx::for_testing(),
                        Duration::from_millis(50),
                    )
                    .await
                    .expect("resumed pane drain"),
                pane
            );
            let successor = authority.activate_pane(19).expect("successor pane");
            assert!(successor.pane_incarnation() > pane.pane_incarnation());
        });
    }

    #[test]
    fn pane_revocation_can_resume_a_dropped_source_transition() {
        run_async_test(async {
            let authority = CaptureAuthority::new();
            let pane = authority.activate_pane(20).expect("pane");
            let lease = authority
                .issue_source(pane, CaptureSourceKind::Polling)
                .expect("source");
            drop(
                authority
                    .begin_source_revocation(pane, lease.stamp())
                    .expect("source revocation handle"),
            );
            let pane_revocation = authority
                .begin_pane_revocation(pane)
                .expect("pane revocation resumes source drain");
            assert_eq!(
                pane_revocation
                    .wait_with_cx(
                        &crate::cx::for_testing(),
                        Duration::from_millis(50),
                    )
                    .await
                    .expect("pane drain"),
                pane
            );
        });
    }

    #[test]
    fn one_lease_allows_only_one_active_drain_waiter() {
        run_async_test(async {
            let authority = CaptureAuthority::new();
            let pane = authority.activate_pane(21).expect("pane");
            let lease = authority
                .issue_source(pane, CaptureSourceKind::Polling)
                .expect("source");
            let revocation = lease
                .begin_or_resume_revocation()
                .expect("revocation handle");
            let waiter = revocation.claim_waiter().expect("first waiter");
            assert_eq!(
                revocation.wait_until_drained().await.unwrap_err(),
                CaptureAuthorityError::TransitionInProgress
            );
            drop(waiter);
            revocation
                .wait_until_drained()
                .await
                .expect("wait after ownership release");
        });
    }

    #[test]
    fn mid_flight_cancellation_is_not_misreported_as_timeout() {
        run_async_test(async {
            let authority = CaptureAuthority::new();
            let pane = authority.activate_pane(22).expect("pane");
            let lease = authority
                .issue_source(pane, CaptureSourceKind::Polling)
                .expect("source");
            let stamp = lease.stamp();
            let held = lease
                .try_acquire_producer(stamp, stamp.global_pane_id())
                .expect("held producer");
            let revocation = authority
                .begin_source_revocation(pane, stamp)
                .expect("revocation");
            let cx = crate::cx::for_testing();
            let timer_cx = crate::cx::for_testing();
            let cancel = async {
                sleep_with_cx(&timer_cx, Duration::from_millis(10))
                    .await
                    .expect("cancel timer");
                cx.cancel_with(
                    crate::outcome::CancelKind::User,
                    Some("capture drain cancellation regression"),
                );
            };
            let (result, ()) = crate::runtime_async::join!(
                revocation.wait_with_cx(&cx, Duration::from_secs(1)),
                cancel
            );
            assert_eq!(
                result.unwrap_err(),
                CaptureAuthorityError::DrainCancelled
            );

            drop(held);
            revocation
                .wait_with_cx(
                    &crate::cx::for_testing(),
                    Duration::from_millis(50),
                )
                .await
                .expect("resume after cancellation");
        });
    }

    #[test]
    fn expired_budget_is_reported_as_timeout_not_direct_cancellation() {
        run_async_test(async {
            let authority = CaptureAuthority::new();
            let pane = authority.activate_pane(23).expect("pane");
            let lease = authority
                .issue_source(pane, CaptureSourceKind::Polling)
                .expect("source");
            let stamp = lease.stamp();
            let held = lease
                .try_acquire_producer(stamp, stamp.global_pane_id())
                .expect("held producer");
            let revocation = authority
                .begin_source_revocation(pane, stamp)
                .expect("revocation");
            let expired = crate::cx::Cx::for_testing_with_budget(
                crate::cx::Budget::new().with_deadline(Default::default()),
            );
            assert_eq!(
                revocation
                    .wait_with_cx(&expired, Duration::from_secs(1))
                    .await
                    .unwrap_err(),
                CaptureAuthorityError::DrainTimedOut
            );
            drop(held);
            revocation
                .wait_with_cx(
                    &crate::cx::for_testing(),
                    Duration::from_millis(50),
                )
                .await
                .expect("resume after budget expiry");
        });
    }

    #[test]
    fn poisoned_authority_fails_closed() {
        let authority = CaptureAuthority::new();
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = authority.lock_state().expect("authority lock");
            panic!("poison capture authority for regression");
        }));
        assert!(poisoned.is_err());
        assert_eq!(
            authority.activate_pane(24).unwrap_err(),
            CaptureAuthorityError::AuthorityPoisoned
        );
    }

    #[test]
    fn inflight_counter_exhaustion_is_typed_and_does_not_wrap() {
        let authority = CaptureAuthority::new();
        let pane = authority.activate_pane(25).expect("pane");
        let lease = authority
            .issue_source(pane, CaptureSourceKind::Polling)
            .expect("source");
        let stamp = lease.stamp();

        lease
            .inner
            .producer_inflight
            .store(usize::MAX, Ordering::SeqCst);
        assert_eq!(
            lease
                .try_acquire_producer(stamp, stamp.global_pane_id())
                .unwrap_err(),
            CaptureAuthorityError::InflightCounterExhausted {
                stage: CaptureGuardStage::Producer,
            }
        );
        assert_eq!(
            lease.inner.producer_inflight.load(Ordering::SeqCst),
            usize::MAX
        );
        lease.inner.producer_inflight.store(0, Ordering::SeqCst);

        lease
            .inner
            .persistence_inflight
            .store(usize::MAX, Ordering::SeqCst);
        assert_eq!(
            lease
                .try_acquire_persistence(stamp, stamp.global_pane_id())
                .unwrap_err(),
            CaptureAuthorityError::InflightCounterExhausted {
                stage: CaptureGuardStage::Persistence,
            }
        );
        assert_eq!(
            lease.inner.persistence_inflight.load(Ordering::SeqCst),
            usize::MAX
        );
        lease
            .inner
            .persistence_inflight
            .store(0, Ordering::SeqCst);
    }

    #[test]
    fn allocation_exhaustion_is_fail_closed_without_partial_installation() {
        let authority = CaptureAuthority::new();
        authority
            .lock_state()
            .expect("authority state")
            .last_incarnation = u64::MAX;
        assert_eq!(
            authority.activate_pane(17).unwrap_err(),
            CaptureAuthorityError::PaneIncarnationExhausted
        );
        assert!(
            !authority
                .lock_state()
                .expect("authority state")
                .panes
                .contains_key(&17)
        );

        authority
            .lock_state()
            .expect("authority state")
            .last_incarnation = 0;
        let pane = authority.activate_pane(17).expect("pane after reset");
        authority
            .lock_state()
            .expect("authority state")
            .last_source_epoch = u64::MAX;
        assert_eq!(
            authority
                .issue_source(pane, CaptureSourceKind::Polling)
                .unwrap_err(),
            CaptureAuthorityError::SourceEpochExhausted
        );
        assert!(
            authority
                .lock_state()
                .expect("authority state")
                .panes
                .get(&17)
                .expect("pane remains")
                .sources
                .is_empty()
        );
    }
}
