//! Runtime authority for capture producer and persistence lifetimes.
//!
//! Numeric pane IDs are reusable.  A queue item identified by only a pane ID
//! can therefore outlive the pane instance that produced it and mutate state
//! belonging to a successor.  This module provides the non-reusable identity
//! and drain protocol used to fence that ABA hazard.

use std::collections::{BTreeMap, HashMap};
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
        counter
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                current.checked_add(1)
            })
            .map_err(|_| CaptureAuthorityError::InflightCounterExhausted { stage })?;

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
    let previous = counter
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
            current.checked_sub(1)
        })
        .expect("capture in-flight guard released exactly once");
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
    accepting_sources: bool,
    sources: BTreeMap<CaptureSourceKind, CaptureLease>,
}

#[derive(Default)]
struct CaptureAuthorityState {
    last_incarnation: u64,
    last_source_epoch: u64,
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
        let mut state = self.lock_state()?;
        if state.panes.contains_key(&global_pane_id) {
            return Err(CaptureAuthorityError::PaneAlreadyActive { global_pane_id });
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
                accepting_sources: true,
                sources: BTreeMap::new(),
            },
        );
        Ok(identity)
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
            assert_eq!(
                authority
                    .begin_source_revocation(pane, stale_stamp)
                    .err()
                    .expect("stale revocation must fail"),
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
                asupersync::Budget::new().with_deadline(asupersync::Time::ZERO),
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
