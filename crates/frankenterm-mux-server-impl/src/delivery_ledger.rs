//! Bounded, level-triggered render-delivery obligations.
//!
//! `ft.render-delivery-ledger.v1` is the contract between mux notification
//! ingress, render snapshot production, wire admission, and eventual
//! application acknowledgement.  The ledger deliberately stores *state*, not
//! wakeups: a wakeup is only a best-effort hint that prompts the dispatch loop
//! to inspect the ledger.  If a bounded wake queue is full, the durable
//! [`DeliveryState::Dirty`] obligation remains present. The single delivery
//! consumer waits through [`DeliveryLedger::poll_claim_next`], which registers
//! its task waker while holding the same exclusive ledger access used by
//! producers. A producer that publishes after the consumer's empty check moves
//! that registration into a pending wake. The owner extracts the wake before
//! releasing exclusive access, then invokes it after releasing the guard. This
//! atomic check/register/publish relation is what prevents a check-then-park
//! lost wake without invoking arbitrary executor code under the owner lock; a
//! best-effort queue edge alone is not sufficient.
//!
//! This module freezes the state and saturation contract.  It does not yet
//! replace the existing dispatch queue, make `PerPane` snapshots transactional,
//! or add wire ACK/NACK messages.  Those integrations must preserve this
//! contract:
//!
//! - render dirties and coalescible state invalidations are durable,
//!   level-triggered obligations;
//! - wake edges are best effort and may be dropped;
//! - lifecycle barriers and true payload events are noncoalescible, but the
//!   current callback returns only subscriber liveness and therefore cannot
//!   reject a producer.  The contract consequently distinguishes future
//!   pre-mutation admission from the journal-plus-resync recovery required for
//!   notifications emitted after authoritative state has already changed;
//! - a successful commit means application acknowledgement, not merely queue
//!   admission or socket write;
//! - a generation-wide resync is an authoritative replacement snapshot and
//!   supersedes queued per-pane dirties, while preserving already in-flight
//!   ordering;
//! - no convergence argument may depend on a later pane event, periodic
//!   polling, or a larger queue.
//!
//! ## Legal v1 transitions
//!
//! | Current state | Input | Next state |
//! |---|---|---|
//! | `Clean` | dirty | `Dirty` |
//! | `Dirty` | dirty | `Dirty` (coalesced) |
//! | `Dirty` | claim | `InFlight { redirtied: false, token }` |
//! | `InFlight { false, token }` | dirty | `InFlight { true, token }` |
//! | `InFlight { true, token }` | dirty | unchanged (coalesced) |
//! | `InFlight { false, token }` | matching commit | `Clean` |
//! | `InFlight { true, token }` | matching commit | `Dirty` at queue tail |
//! | `InFlight { _, token }` | matching retry | `Dirty` at queue tail |
//! | any pane state | pane close | `Closed` |
//! | `Closed` | repeated pane close before ACK | `Closed` and return the same close capability |
//! | `Closed` | exact lifecycle application ACK | tombstone reclaimed; allocator identity remains non-reusable |
//! | any generation state | shutdown/terminal failure | `Closed` |
//! | `Closed` | any later input | `Closed` |
//!
//! A stale, duplicate, wrong-scope, or wrong-generation settlement never
//! changes state.  `resync_all` uses the same four states.  Requesting it from
//! `Clean` supersedes queued pane dirties; older pane claims must settle before
//! it can be claimed.  A dirty observed while the resync is in flight sets its
//! `redirtied` bit, requiring a fresh resync after commit.
//!
//! Reclaiming a pane tombstone requires the exact [`PaneCloseAckToken`] minted
//! by that close. After application acknowledgement, the ledger may forget the
//! tombstone because [`mux::pane::alloc_pane_id`] is the process-lifetime
//! identity authority: it allocates monotonically, never reuses an ID, and
//! fails before exhaustion. The ledger deliberately does not infer liveness or
//! retirement from numeric order. A lower ID may be a still-live pane first
//! observed after a higher pane closed; treating it as retired would create a
//! false generation failure. Authoritative bootstrap and live integration must
//! admit only allocator-issued IDs and reconcile them against the mux
//! inventory.
//!
//! | Notification effect | Ordering/coalescing | Saturation recovery |
//! |---|---|---|
//! | render invalidation | per-pane causal; one dirty bit; pane round-robin | keep obligation, drop only the payload-free scheduler wake |
//! | lifecycle/topology barrier | strict FIFO; never coalesce | pre-admit before mutation, or durably journal then force an authoritative topology resync |
//! | state invalidation | per-key causal; latest value; key round-robin | keep obligation in a bounded level store |
//! | true event | generation-causal FIFO; never coalesce | pre-admit or durably journal the occurrence |
//! | large external command | strict FIFO plus byte budget | pre-admit or spool durably; item-count capacity is insufficient |
//! | derived level predicate | recheck authoritative level | retain the predicate; it is not a droppable wake |
//! | scheduler wake | no ordering or semantic payload | drop/coalesce the edge |
//!
//! Cross-class priority is bounded by [`DELIVERY_PRIORITY_BURST_LIMIT`];
//! priority cannot starve any ready durable class.  A closed semantic queue
//! rejects the producer and closes the local generation.  Only a payload-free
//! wake edge may be ignored after close.

use mux::MuxNotification;
use mux::client::ClientId;
use mux::pane::PaneId;
use mux::tab::TabId;
use mux::window::WindowId;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::task::{Context, Poll, Waker};

/// Stable identifier cited by all render-delivery implementation children.
pub const DELIVERY_LEDGER_CONTRACT_VERSION: &str = "ft.render-delivery-ledger.v1";
/// Maximum consecutive items from a higher priority band while a lower
/// durable priority band is ready.  Intra-class ordering still follows the
/// class-specific fairness contract.
pub const DELIVERY_PRIORITY_BURST_LIMIT: usize = 8;

static NEXT_DELIVERY_LEDGER_INSTANCE: AtomicU64 = AtomicU64::new(1);

/// A consumer wake extracted from a delivery owner for deferred execution.
///
/// Ledger and coordinator mutation is normally serialized by an external
/// mutex. Arbitrary [`Waker`] implementations may synchronously re-enter the
/// consumer, so calling `wake` while that mutex is held can deadlock. Callers
/// must extract this value through the owner's `take_pending_wake` method while
/// holding exclusive ownership, release the external guard, and only then call
/// [`Self::wake`].
#[must_use = "release the delivery owner guard, then call PendingDeliveryWake::wake"]
#[derive(Debug)]
pub struct PendingDeliveryWake(Waker);

impl PendingDeliveryWake {
    pub(crate) fn new(waker: Waker) -> Self {
        Self(waker)
    }

    /// Wake the registered consumer after releasing the delivery owner guard.
    pub fn wake(self) {
        self.0.wake();
    }
}

/// Process-local, never-reused identity for one concrete ledger instance.
///
/// A caller-controlled [`DeliveryGeneration`] can legitimately recur after a
/// teardown/reconnect bug or replay. Capabilities therefore bind both the
/// semantic generation and this constructor-minted incarnation.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DeliveryLedgerInstance(u64);

impl DeliveryLedgerInstance {
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    #[cfg(test)]
    pub(crate) const fn for_test(value: u64) -> Self {
        Self(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeliveryLedgerInstanceExhausted;

impl std::fmt::Display for DeliveryLedgerInstanceExhausted {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(
            "delivery-ledger instance identity space is exhausted; refusing to wrap or reuse",
        )
    }
}

impl std::error::Error for DeliveryLedgerInstanceExhausted {}

fn allocate_delivery_ledger_instance(
    counter: &AtomicU64,
) -> Result<DeliveryLedgerInstance, DeliveryLedgerInstanceExhausted> {
    let mut current = counter.load(AtomicOrdering::Relaxed);
    loop {
        if current == 0 {
            return Err(DeliveryLedgerInstanceExhausted);
        }
        let next = current
            .checked_add(1)
            .ok_or(DeliveryLedgerInstanceExhausted)?;
        match counter.compare_exchange_weak(
            current,
            next,
            AtomicOrdering::Relaxed,
            AtomicOrdering::Relaxed,
        ) {
            Ok(_) => return Ok(DeliveryLedgerInstance(current)),
            Err(observed) => current = observed,
        }
    }
}

/// Connection-local generation identity.
///
/// This identity prevents accidental cross-generation token settlement inside
/// the contract.  It is intentionally not a wire/reconnect generation; that
/// protocol is owned by the reconnect/bootstrap layer.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DeliveryGeneration(u64);

impl DeliveryGeneration {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Monotonic, never-reused token for one claimed delivery obligation.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DeliveryToken {
    instance: DeliveryLedgerInstance,
    generation: DeliveryGeneration,
    sequence: u64,
}

impl DeliveryToken {
    #[must_use]
    pub const fn instance(self) -> DeliveryLedgerInstance {
        self.instance
    }

    #[must_use]
    pub const fn generation(self) -> DeliveryGeneration {
        self.generation
    }

    #[must_use]
    pub const fn sequence(self) -> u64 {
        self.sequence
    }
}

/// Durable state of a pane or the generation-wide resync slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeliveryState {
    /// No delivery is currently required.
    Clean,
    /// Delivery is required and has not been claimed.
    Dirty,
    /// A delivery was claimed. `redirtied` records a later invalidation that
    /// the claimed snapshot cannot cover.
    InFlight {
        redirtied: bool,
        token: DeliveryToken,
    },
    /// The pane or whole local generation is terminal.
    Closed,
}

/// Scope covered by a delivery claim.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DeliveryScope {
    Pane(PaneId),
    ResyncAll,
}

/// Capability returned to the delivery producer.
///
/// The exact claim must be supplied when committing, retrying, or reporting a
/// terminal failure.  A stale or duplicate claim cannot settle newer work.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DeliveryClaim {
    scope: DeliveryScope,
    token: DeliveryToken,
}

impl DeliveryClaim {
    #[must_use]
    pub const fn scope(self) -> DeliveryScope {
        self.scope
    }

    #[must_use]
    pub const fn token(self) -> DeliveryToken {
        self.token
    }
}

/// Exact capability that may reclaim one closed-pane tombstone.
///
/// The lifecycle producer sends this opaque token with the close barrier, and
/// the consumer returns it only after applying that barrier. Possession is not
/// queue-admission or socket-write authority: callers must invoke
/// [`DeliveryLedger::acknowledge_pane_close`] only from the application-ACK
/// path.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PaneCloseAckToken {
    instance: DeliveryLedgerInstance,
    generation: DeliveryGeneration,
    pane_id: PaneId,
    sequence: u64,
}

impl PaneCloseAckToken {
    #[must_use]
    pub const fn instance(self) -> DeliveryLedgerInstance {
        self.instance
    }

    #[must_use]
    pub const fn generation(self) -> DeliveryGeneration {
        self.generation
    }

    #[must_use]
    pub const fn pane_id(self) -> PaneId {
        self.pane_id
    }

    #[must_use]
    pub const fn sequence(self) -> u64 {
        self.sequence
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DirtyOutcome {
    BecameDirty,
    MarkedInFlightRedirty,
    Coalesced,
    CoveredByResyncAll,
    EscalatedToResyncAll,
    IgnoredClosed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResyncOutcome {
    Requested,
    MarkedInFlightRedirty,
    Coalesced,
    IgnoredClosed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use = "delivery settlement outcomes must be checked to preserve durable obligation state"]
pub enum SettleOutcome {
    CommittedClean,
    LocallySettledNoChange,
    RequeuedDirty,
    SupersededByResyncAll,
    StaleOrDuplicate,
    ScopeClosed,
    GenerationClosed,
    FailedClosed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use = "pane close returns the exact application-ACK capability required to reclaim its tombstone"]
pub enum ClosePaneOutcome {
    ClosedClean {
        close_ack: PaneCloseAckToken,
    },
    ClosedDirty {
        close_ack: PaneCloseAckToken,
    },
    ClosedInFlight {
        delivery_token: DeliveryToken,
        close_ack: PaneCloseAckToken,
    },
    AlreadyClosed {
        close_ack: PaneCloseAckToken,
    },
    Untracked,
    GenerationClosed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use = "pane reclamation outcomes must be checked so tombstones are not stranded"]
pub enum ReclaimPaneOutcome {
    Reclaimed,
    AwaitingClose,
    WrongInstance,
    WrongGeneration,
    WrongPane,
    StaleOrDuplicate,
    GenerationClosed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClaimError {
    /// The monotonic token space is exhausted.  The entire local generation is
    /// closed before returning this error; token wrap and reuse are forbidden.
    TokenExhausted,
}

/// Monotonic observability counters required by the v1 contract.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DeliveryCounters {
    /// Calls that attempted to publish a pane invalidation.
    pub dirties: u64,
    /// Dirty publications ignored because the pane or generation was closed.
    pub ignored_dirties: u64,
    /// Publications absorbed by an already-dirty obligation.
    pub coalesces: u64,
    /// Per-pane table capacity overflows promoted to `resync_all`.
    pub overflows: u64,
    /// Explicit or overflow-triggered requests for an authoritative resync.
    pub resyncs: u64,
    /// Authoritative resync obligations claimed for preparation.
    pub resync_claims: u64,
    /// Application-acknowledged deliveries.
    pub commits: u64,
    /// Pure preparations that proved there was no application-visible change.
    pub no_change_settlements: u64,
    /// Transient delivery failures returned to dirty state.
    pub retries: u64,
    /// Same-generation failures that terminally closed the delivery lane.
    pub terminal_failures: u64,
    /// Consumer wakers registered at an atomic empty-check boundary.
    pub wake_registrations: u64,
    /// Registered-consumer wakes extracted for delivery by the owner.
    pub wake_deliveries: u64,
    /// Registered-consumer wakes deliberately suppressed because a valid
    /// publication or settlement did not change work from unclaimable to
    /// claimable. The waiter remains installed for the fence-clearing event.
    pub suppressed_wakes: u64,
    /// Exact close acknowledgements that reclaimed a bounded tombstone.
    pub close_acknowledgements: u64,
    /// Early, wrong-generation, wrong-pane, stale, duplicate, untracked, or
    /// generation-closed close acknowledgement attempts.
    pub rejected_close_acknowledgements: u64,
    /// Settlement attempts carrying a capability minted by another concrete
    /// ledger instance, even when the caller-controlled generation is equal.
    pub rejected_cross_instance_settlements: u64,
    /// Close acknowledgements carrying a capability minted by another
    /// concrete ledger instance.
    pub rejected_cross_instance_close_acknowledgements: u64,
}

impl DeliveryCounters {
    fn increment(value: &mut u64) {
        *value = value.saturating_add(1);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeliveryCapacity {
    pub pane_limit: usize,
    pub tracked_panes: usize,
    pub ready_panes: usize,
    /// Exactly one fixed-size generation-wide escape-hatch slot is retained.
    pub generation_slots: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PaneEntry {
    state: DeliveryState,
    queued: bool,
    close_ack: Option<PaneCloseAckToken>,
}

/// Bounded render-delivery state for one connection-local generation.
///
/// Memory is `O(pane_limit) + O(1)`: at most `pane_limit` map entries and
/// `pane_limit` ready-queue entries plus one `resync_all` state slot.  A zero
/// pane limit is valid and routes every render dirty to `resync_all`.
#[derive(Debug)]
pub struct DeliveryLedger {
    instance: DeliveryLedgerInstance,
    generation: DeliveryGeneration,
    pane_limit: usize,
    panes: HashMap<PaneId, PaneEntry>,
    ready: VecDeque<PaneId>,
    inflight_panes: usize,
    resync_all: DeliveryState,
    next_token_sequence: Option<u64>,
    next_close_ack_sequence: Option<u64>,
    counters: DeliveryCounters,
    waiter: Option<Waker>,
    pending_wake: Option<Waker>,
}

impl DeliveryLedger {
    pub fn new(
        generation: DeliveryGeneration,
        pane_limit: usize,
    ) -> Result<Self, DeliveryLedgerInstanceExhausted> {
        let instance = allocate_delivery_ledger_instance(&NEXT_DELIVERY_LEDGER_INSTANCE)?;
        Ok(Self {
            instance,
            generation,
            pane_limit,
            // Do not trust a configuration-sized capacity as an eager
            // allocation request. Both collections grow only as obligations
            // are observed and remain bounded by `pane_limit`.
            panes: HashMap::new(),
            ready: VecDeque::new(),
            inflight_panes: 0,
            resync_all: DeliveryState::Clean,
            next_token_sequence: Some(1),
            next_close_ack_sequence: Some(1),
            counters: DeliveryCounters::default(),
            waiter: None,
            pending_wake: None,
        })
    }

    #[must_use]
    pub const fn instance(&self) -> DeliveryLedgerInstance {
        self.instance
    }

    #[must_use]
    pub const fn generation(&self) -> DeliveryGeneration {
        self.generation
    }

    #[must_use]
    pub const fn counters(&self) -> DeliveryCounters {
        self.counters
    }

    #[must_use]
    pub fn capacity(&self) -> DeliveryCapacity {
        DeliveryCapacity {
            pane_limit: self.pane_limit,
            tracked_panes: self.panes.len(),
            ready_panes: self.ready.len(),
            generation_slots: 1,
        }
    }

    #[must_use]
    pub fn pane_state(&self, pane_id: PaneId) -> Option<DeliveryState> {
        self.panes.get(&pane_id).map(|entry| entry.state)
    }

    #[must_use]
    pub const fn resync_all_state(&self) -> DeliveryState {
        self.resync_all
    }

    #[must_use]
    pub const fn is_closed(&self) -> bool {
        matches!(self.resync_all, DeliveryState::Closed)
    }

    /// Record a durable pane invalidation.
    ///
    /// Callers may separately attempt to enqueue a best-effort queue edge, but
    /// correctness comes from the waker registered by
    /// [`Self::poll_claim_next`]. A failed queue enqueue must not modify or
    /// roll back this transition.
    pub fn mark_dirty(&mut self, pane_id: PaneId) -> DirtyOutcome {
        let was_claimable = self.has_claimable_work();
        let outcome = self.mark_dirty_inner(pane_id);
        match outcome {
            DirtyOutcome::IgnoredClosed => {
                DeliveryCounters::increment(&mut self.counters.ignored_dirties);
            }
            DirtyOutcome::BecameDirty
            | DirtyOutcome::MarkedInFlightRedirty
            | DirtyOutcome::Coalesced
            | DirtyOutcome::CoveredByResyncAll
            | DirtyOutcome::EscalatedToResyncAll => {
                self.wake_if_newly_claimable(was_claimable);
            }
        }
        outcome
    }

    fn mark_dirty_inner(&mut self, pane_id: PaneId) -> DirtyOutcome {
        DeliveryCounters::increment(&mut self.counters.dirties);

        if self.is_closed() {
            return DirtyOutcome::IgnoredClosed;
        }
        if self
            .panes
            .get(&pane_id)
            .is_some_and(|entry| entry.state == DeliveryState::Closed)
        {
            return DirtyOutcome::IgnoredClosed;
        }
        match self.resync_all {
            DeliveryState::Closed => return DirtyOutcome::IgnoredClosed,
            DeliveryState::Dirty => {
                DeliveryCounters::increment(&mut self.counters.coalesces);
                return DirtyOutcome::CoveredByResyncAll;
            }
            DeliveryState::InFlight {
                redirtied: false,
                token,
            } => {
                self.resync_all = DeliveryState::InFlight {
                    redirtied: true,
                    token,
                };
                return DirtyOutcome::MarkedInFlightRedirty;
            }
            DeliveryState::InFlight {
                redirtied: true, ..
            } => {
                DeliveryCounters::increment(&mut self.counters.coalesces);
                return DirtyOutcome::CoveredByResyncAll;
            }
            DeliveryState::Clean => {}
        }

        if let Some(entry) = self.panes.get_mut(&pane_id) {
            return match entry.state {
                DeliveryState::Clean => {
                    entry.state = DeliveryState::Dirty;
                    entry.queued = true;
                    self.ready.push_back(pane_id);
                    DirtyOutcome::BecameDirty
                }
                DeliveryState::Dirty => {
                    DeliveryCounters::increment(&mut self.counters.coalesces);
                    DirtyOutcome::Coalesced
                }
                DeliveryState::InFlight {
                    redirtied: false,
                    token,
                } => {
                    entry.state = DeliveryState::InFlight {
                        redirtied: true,
                        token,
                    };
                    DirtyOutcome::MarkedInFlightRedirty
                }
                DeliveryState::InFlight {
                    redirtied: true, ..
                } => {
                    DeliveryCounters::increment(&mut self.counters.coalesces);
                    DirtyOutcome::Coalesced
                }
                DeliveryState::Closed => DirtyOutcome::IgnoredClosed,
            };
        }

        if self.panes.len() >= self.pane_limit {
            DeliveryCounters::increment(&mut self.counters.overflows);
            let _ = self.request_resync_all_inner();
            return DirtyOutcome::EscalatedToResyncAll;
        }

        self.panes.insert(
            pane_id,
            PaneEntry {
                state: DeliveryState::Dirty,
                queued: true,
                close_ack: None,
            },
        );
        self.ready.push_back(pane_id);
        DirtyOutcome::BecameDirty
    }

    /// Request an authoritative snapshot for every live pane in this local
    /// generation.
    ///
    /// A pending resync supersedes queued pane dirties.  Existing in-flight
    /// pane claims retain their tokens so the eventual resync cannot overtake
    /// them on a FIFO wire/application lane.
    pub fn request_resync_all(&mut self) -> ResyncOutcome {
        let was_claimable = self.has_claimable_work();
        let outcome = self.request_resync_all_inner();
        if outcome != ResyncOutcome::IgnoredClosed {
            self.wake_if_newly_claimable(was_claimable);
        }
        outcome
    }

    fn request_resync_all_inner(&mut self) -> ResyncOutcome {
        match self.resync_all {
            DeliveryState::Closed => ResyncOutcome::IgnoredClosed,
            DeliveryState::Dirty => {
                DeliveryCounters::increment(&mut self.counters.coalesces);
                ResyncOutcome::Coalesced
            }
            DeliveryState::InFlight {
                redirtied: false,
                token,
            } => {
                DeliveryCounters::increment(&mut self.counters.resyncs);
                self.resync_all = DeliveryState::InFlight {
                    redirtied: true,
                    token,
                };
                ResyncOutcome::MarkedInFlightRedirty
            }
            DeliveryState::InFlight {
                redirtied: true, ..
            } => {
                DeliveryCounters::increment(&mut self.counters.coalesces);
                ResyncOutcome::Coalesced
            }
            DeliveryState::Clean => {
                DeliveryCounters::increment(&mut self.counters.resyncs);
                self.resync_all = DeliveryState::Dirty;
                self.ready.clear();
                for entry in self.panes.values_mut() {
                    entry.queued = false;
                    match entry.state {
                        DeliveryState::Dirty => entry.state = DeliveryState::Clean,
                        DeliveryState::InFlight { token, .. } => {
                            // A future authoritative snapshot covers any
                            // redirty attached to this older in-flight claim.
                            entry.state = DeliveryState::InFlight {
                                redirtied: false,
                                token,
                            };
                        }
                        DeliveryState::Clean | DeliveryState::Closed => {}
                    }
                }
                ResyncOutcome::Requested
            }
        }
    }

    /// Claim the next durable obligation.
    ///
    /// `resync_all` has supersession priority, but cannot overtake older
    /// in-flight pane claims.  Otherwise pane claims are FIFO round-robin:
    /// work redirtied while in flight is appended at the tail when settled.
    pub fn claim_next(&mut self) -> Result<Option<DeliveryClaim>, ClaimError> {
        if self.is_closed() {
            return Ok(None);
        }

        if matches!(self.resync_all, DeliveryState::Dirty) {
            if self.inflight_panes != 0 {
                return Ok(None);
            }
            let token = self.issue_token()?;
            // There is one logical consumer. Only an actual successful claim
            // supersedes its prior wait registration or not-yet-delivered
            // pending wake. An empty direct probe must preserve the waiter.
            self.waiter = None;
            self.pending_wake = None;
            self.resync_all = DeliveryState::InFlight {
                redirtied: false,
                token,
            };
            DeliveryCounters::increment(&mut self.counters.resync_claims);
            return Ok(Some(DeliveryClaim {
                scope: DeliveryScope::ResyncAll,
                token,
            }));
        }

        if matches!(self.resync_all, DeliveryState::InFlight { .. }) {
            return Ok(None);
        }

        while let Some(pane_id) = self.ready.pop_front() {
            let Some(entry) = self.panes.get(&pane_id) else {
                continue;
            };
            if entry.state != DeliveryState::Dirty {
                if let Some(entry) = self.panes.get_mut(&pane_id) {
                    entry.queued = false;
                }
                continue;
            }
            let token = self.issue_token()?;
            self.waiter = None;
            self.pending_wake = None;
            let Some(entry) = self.panes.get_mut(&pane_id) else {
                // The ledger is single-owner and token issuance cannot mutate
                // the pane map. Keep this defensive branch non-panicking if a
                // future implementation changes that assumption; a skipped
                // token is safe because tokens are never reused.
                continue;
            };
            entry.queued = false;
            entry.state = DeliveryState::InFlight {
                redirtied: false,
                token,
            };
            self.inflight_panes += 1;
            return Ok(Some(DeliveryClaim {
                scope: DeliveryScope::Pane(pane_id),
                token,
            }));
        }

        Ok(None)
    }

    /// Poll the next durable obligation without a check-then-park race.
    ///
    /// The ledger must be mutated through the same exclusive owner (normally a
    /// mutex guard) for both producer transitions and this poll. If no claim is
    /// ready, the waker is installed before that exclusive access is released.
    /// A later state transition defers it for [`Self::take_pending_wake`] even
    /// when every bounded best-effort wake queue has capacity zero.
    pub fn poll_claim_next(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<DeliveryClaim>, ClaimError>> {
        match self.claim_next() {
            Ok(None) if !self.is_closed() => {
                self.waiter = Some(cx.waker().clone());
                DeliveryCounters::increment(&mut self.counters.wake_registrations);
                Poll::Pending
            }
            result => Poll::Ready(result),
        }
    }

    /// Extract a registered consumer wake while retaining exclusive ownership.
    ///
    /// Every caller that performs a mutation which can make work claimable or
    /// close the generation must call this before releasing its external owner
    /// guard. This includes claim/error paths that can close on token
    /// exhaustion. The returned wake must not be fired until after that guard
    /// is released. Extraction transfers the single pending wake obligation
    /// and accounts it as a delivery; dropping the returned value without
    /// waking violates the owner contract.
    pub fn take_pending_wake(&mut self) -> Option<PendingDeliveryWake> {
        let waiter = self.pending_wake.take()?;
        DeliveryCounters::increment(&mut self.counters.wake_deliveries);
        Some(PendingDeliveryWake::new(waiter))
    }

    /// Commit an application-acknowledged claim.
    pub fn commit(&mut self, claim: DeliveryClaim) -> SettleOutcome {
        if claim.token.instance != self.instance {
            DeliveryCounters::increment(&mut self.counters.rejected_cross_instance_settlements);
            return SettleOutcome::StaleOrDuplicate;
        }
        if self.is_closed() {
            return SettleOutcome::GenerationClosed;
        }
        if claim.token.generation != self.generation {
            return SettleOutcome::StaleOrDuplicate;
        }

        let was_claimable = self.has_claimable_work();
        let outcome = match claim.scope {
            DeliveryScope::ResyncAll => self.commit_resync_all(claim.token),
            DeliveryScope::Pane(pane_id) => self.commit_pane(pane_id, claim.token),
        };
        if !matches!(
            outcome,
            SettleOutcome::StaleOrDuplicate
                | SettleOutcome::ScopeClosed
                | SettleOutcome::GenerationClosed
        ) {
            self.wake_if_newly_claimable(was_claimable);
        }
        outcome
    }

    /// Settle a pane claim whose pure preparation produced no PDU and no
    /// application-side effect.
    ///
    /// This is deliberately distinct from [`Self::commit`]: it cannot be used
    /// for `resync_all`, cannot install a changed baseline, and cannot consume
    /// alerts, palette transitions, or any other effect that would require an
    /// application acknowledgement.
    pub fn settle_no_change(&mut self, claim: DeliveryClaim) -> SettleOutcome {
        if claim.token.instance != self.instance {
            DeliveryCounters::increment(&mut self.counters.rejected_cross_instance_settlements);
            return SettleOutcome::StaleOrDuplicate;
        }
        if self.is_closed() {
            return SettleOutcome::GenerationClosed;
        }
        if claim.token.generation != self.generation {
            return SettleOutcome::StaleOrDuplicate;
        }
        let DeliveryScope::Pane(pane_id) = claim.scope else {
            return SettleOutcome::StaleOrDuplicate;
        };
        let was_claimable = self.has_claimable_work();
        let Some(entry) = self.panes.get_mut(&pane_id) else {
            return SettleOutcome::StaleOrDuplicate;
        };
        let DeliveryState::InFlight { redirtied, token } = entry.state else {
            return if entry.state == DeliveryState::Closed {
                SettleOutcome::ScopeClosed
            } else {
                SettleOutcome::StaleOrDuplicate
            };
        };
        if token != claim.token {
            return SettleOutcome::StaleOrDuplicate;
        }

        DeliveryCounters::increment(&mut self.counters.no_change_settlements);
        self.inflight_panes -= 1;
        let outcome = if self.resync_all != DeliveryState::Clean {
            entry.state = DeliveryState::Clean;
            entry.queued = false;
            SettleOutcome::SupersededByResyncAll
        } else if redirtied {
            entry.state = DeliveryState::Dirty;
            entry.queued = true;
            self.ready.push_back(pane_id);
            SettleOutcome::RequeuedDirty
        } else {
            entry.state = DeliveryState::Clean;
            entry.queued = false;
            SettleOutcome::LocallySettledNoChange
        };
        self.wake_if_newly_claimable(was_claimable);
        outcome
    }

    /// Return a transiently failed claim to durable dirty state.
    pub fn retry(&mut self, claim: DeliveryClaim) -> SettleOutcome {
        let was_claimable = self.has_claimable_work();
        let outcome = self.retry_inner(claim);
        if !matches!(
            outcome,
            SettleOutcome::StaleOrDuplicate
                | SettleOutcome::ScopeClosed
                | SettleOutcome::GenerationClosed
        ) {
            self.wake_if_newly_claimable(was_claimable);
        }
        outcome
    }

    fn retry_inner(&mut self, claim: DeliveryClaim) -> SettleOutcome {
        if claim.token.instance != self.instance {
            DeliveryCounters::increment(&mut self.counters.rejected_cross_instance_settlements);
            return SettleOutcome::StaleOrDuplicate;
        }
        if self.is_closed() {
            return SettleOutcome::GenerationClosed;
        }
        if claim.token.generation != self.generation {
            return SettleOutcome::StaleOrDuplicate;
        }

        match claim.scope {
            DeliveryScope::ResyncAll => {
                let DeliveryState::InFlight { token, .. } = self.resync_all else {
                    return SettleOutcome::StaleOrDuplicate;
                };
                if token != claim.token {
                    return SettleOutcome::StaleOrDuplicate;
                }
                DeliveryCounters::increment(&mut self.counters.retries);
                self.resync_all = DeliveryState::Dirty;
                SettleOutcome::RequeuedDirty
            }
            DeliveryScope::Pane(pane_id) => {
                let Some(entry) = self.panes.get_mut(&pane_id) else {
                    return SettleOutcome::StaleOrDuplicate;
                };
                let DeliveryState::InFlight { token, .. } = entry.state else {
                    return if entry.state == DeliveryState::Closed {
                        SettleOutcome::ScopeClosed
                    } else {
                        SettleOutcome::StaleOrDuplicate
                    };
                };
                if token != claim.token {
                    return SettleOutcome::StaleOrDuplicate;
                }
                DeliveryCounters::increment(&mut self.counters.retries);
                self.inflight_panes -= 1;
                if self.resync_all == DeliveryState::Clean {
                    entry.state = DeliveryState::Dirty;
                    entry.queued = true;
                    self.ready.push_back(pane_id);
                    SettleOutcome::RequeuedDirty
                } else {
                    entry.state = DeliveryState::Clean;
                    entry.queued = false;
                    SettleOutcome::SupersededByResyncAll
                }
            }
        }
    }

    /// Fail the entire local generation after a terminal transport error.
    ///
    /// Same-generation transport death is unconditional: a pane close or
    /// logical settlement racing ahead of the error cannot keep a dead
    /// connection open. Wrong-generation reports remain stale and cannot
    /// close a newly established ledger. Reconnect and authoritative bootstrap
    /// are responsible for creating a new ledger.
    pub fn fail_terminal(&mut self, claim: DeliveryClaim) -> SettleOutcome {
        if claim.token.instance != self.instance {
            DeliveryCounters::increment(&mut self.counters.rejected_cross_instance_settlements);
            return SettleOutcome::StaleOrDuplicate;
        }
        if self.is_closed() {
            return SettleOutcome::GenerationClosed;
        }
        if claim.token.generation != self.generation {
            return SettleOutcome::StaleOrDuplicate;
        }
        DeliveryCounters::increment(&mut self.counters.terminal_failures);
        self.close_generation();
        SettleOutcome::FailedClosed
    }

    /// Close a pane, invalidate any outstanding render claim, and mint the
    /// exact capability required to reclaim its tombstone after application
    /// acknowledgement.
    pub fn close_pane(&mut self, pane_id: PaneId) -> ClosePaneOutcome {
        if self.is_closed() {
            return ClosePaneOutcome::GenerationClosed;
        }

        let Some(prior_state) = self.panes.get(&pane_id).map(|entry| entry.state) else {
            // An all-pane snapshot inventories panes that never occupied a
            // bounded per-pane slot. Closing one of those panes can invalidate
            // an already-produced resync snapshot, so preserve the close as a
            // fresh resync obligation even though the pane itself is untracked.
            let was_claimable = self.has_claimable_work();
            if self.redirty_inflight_resync() {
                self.wake_if_newly_claimable(was_claimable);
            }
            return ClosePaneOutcome::Untracked;
        };
        if prior_state == DeliveryState::Closed {
            let Some(close_ack) = self.panes.get(&pane_id).and_then(|entry| entry.close_ack) else {
                // Live-generation invariants require a capability on every
                // tracked closed pane. If that invariant is ever broken,
                // losing the only reclaim authority must fail closed.
                DeliveryCounters::increment(&mut self.counters.terminal_failures);
                self.close_generation();
                return ClosePaneOutcome::GenerationClosed;
            };
            return ClosePaneOutcome::AlreadyClosed { close_ack };
        }

        let Some(close_ack) = self.issue_pane_close_ack(pane_id) else {
            return ClosePaneOutcome::GenerationClosed;
        };
        let was_claimable = self.has_claimable_work();
        let outcome = match prior_state {
            DeliveryState::Clean => ClosePaneOutcome::ClosedClean { close_ack },
            DeliveryState::Dirty => ClosePaneOutcome::ClosedDirty { close_ack },
            DeliveryState::InFlight { token, .. } => {
                self.inflight_panes -= 1;
                ClosePaneOutcome::ClosedInFlight {
                    delivery_token: token,
                    close_ack,
                }
            }
            DeliveryState::Closed => {
                DeliveryCounters::increment(&mut self.counters.terminal_failures);
                self.close_generation();
                return ClosePaneOutcome::GenerationClosed;
            }
        };
        let Some(entry) = self.panes.get_mut(&pane_id) else {
            // The ledger is single-owner and close-token issuance cannot
            // mutate the pane map. Preserve fail-closed behavior if a future
            // implementation invalidates that assumption.
            DeliveryCounters::increment(&mut self.counters.terminal_failures);
            self.close_generation();
            return ClosePaneOutcome::GenerationClosed;
        };
        entry.state = DeliveryState::Closed;
        entry.queued = false;
        entry.close_ack = Some(close_ack);
        self.ready.retain(|queued| *queued != pane_id);

        // If an all-pane snapshot is already in flight, its inventory may
        // predate this close. Force a second authoritative pass.
        self.redirty_inflight_resync();
        // Closing an in-flight pane wakes only when it removes the last
        // ordering fence in front of a dirty resync. Redirtying an in-flight
        // resync remains fenced by its older claim and deliberately retains
        // the registered waiter without waking it.
        self.wake_if_newly_claimable(was_claimable);
        outcome
    }

    /// Reclaim a closed-pane tombstone after its strict FIFO lifecycle barrier
    /// has been application-acknowledged.
    ///
    /// The caller must not invoke this at queue admission or socket-write
    /// time. The exact generation-, pane-, and close-bound token is the causal
    /// fence proving that older render work for this pane can no longer arrive.
    /// Successful reclamation releases only the bounded ledger tombstone.
    /// Process-lifetime non-reuse is guaranteed by the mux PaneId allocator,
    /// not by retaining an unbounded history or comparing numeric ID order.
    pub fn acknowledge_pane_close(
        &mut self,
        pane_id: PaneId,
        close_ack: PaneCloseAckToken,
    ) -> ReclaimPaneOutcome {
        let outcome = if close_ack.instance != self.instance {
            DeliveryCounters::increment(
                &mut self.counters.rejected_cross_instance_close_acknowledgements,
            );
            ReclaimPaneOutcome::WrongInstance
        } else if self.is_closed() {
            ReclaimPaneOutcome::GenerationClosed
        } else if close_ack.generation != self.generation {
            ReclaimPaneOutcome::WrongGeneration
        } else if close_ack.pane_id != pane_id {
            ReclaimPaneOutcome::WrongPane
        } else {
            match self.panes.get(&pane_id) {
                Some(PaneEntry {
                    state: DeliveryState::Closed,
                    close_ack: Some(expected),
                    ..
                }) if *expected == close_ack => {
                    self.panes.remove(&pane_id);
                    DeliveryCounters::increment(&mut self.counters.close_acknowledgements);
                    return ReclaimPaneOutcome::Reclaimed;
                }
                Some(PaneEntry {
                    state: DeliveryState::Closed,
                    ..
                }) => ReclaimPaneOutcome::StaleOrDuplicate,
                Some(PaneEntry {
                    state:
                        DeliveryState::Clean | DeliveryState::Dirty | DeliveryState::InFlight { .. },
                    ..
                }) => ReclaimPaneOutcome::AwaitingClose,
                None => ReclaimPaneOutcome::StaleOrDuplicate,
            }
        };
        DeliveryCounters::increment(&mut self.counters.rejected_close_acknowledgements);
        outcome
    }

    /// Terminally close this connection-local generation.
    pub fn close_generation(&mut self) {
        self.resync_all = DeliveryState::Closed;
        self.ready.clear();
        self.inflight_panes = 0;
        for entry in self.panes.values_mut() {
            entry.state = DeliveryState::Closed;
            entry.queued = false;
        }
        self.defer_waiter_wake();
    }

    /// Validate capacity, queue, token, and shutdown invariants.
    pub fn check_invariants(&self) -> Result<(), &'static str> {
        if self.panes.len() > self.pane_limit {
            return Err("tracked pane count exceeds configured capacity");
        }
        if self.ready.len() > self.panes.len() {
            return Err("ready queue exceeds tracked pane count");
        }

        let mut queued = HashSet::with_capacity(self.ready.len());
        for pane_id in &self.ready {
            if !queued.insert(*pane_id) {
                return Err("ready queue contains a duplicate pane");
            }
            let Some(entry) = self.panes.get(pane_id) else {
                return Err("ready queue references an untracked pane");
            };
            if entry.state != DeliveryState::Dirty || !entry.queued {
                return Err("ready queue references a non-dirty pane");
            }
        }

        let global_is_clean = self.resync_all == DeliveryState::Clean;
        let mut tokens = HashSet::new();
        let mut close_ack_sequences = HashSet::new();
        let mut observed_inflight_panes = 0usize;
        for (pane_id, entry) in &self.panes {
            if entry.queued != queued.contains(pane_id) {
                return Err("pane queued bit disagrees with ready queue");
            }
            if matches!(entry.state, DeliveryState::Dirty) != entry.queued {
                return Err("dirty pane must appear exactly once in ready queue");
            }
            if !global_is_clean && entry.state == DeliveryState::Dirty {
                return Err("resync-all must supersede queued pane dirties");
            }
            match (entry.state, entry.close_ack) {
                (DeliveryState::Closed, Some(close_ack)) => {
                    if close_ack.instance != self.instance
                        || close_ack.generation != self.generation
                        || close_ack.pane_id != *pane_id
                    {
                        return Err("pane close acknowledgement token has wrong scope");
                    }
                    if !close_ack_sequences.insert(close_ack.sequence) {
                        return Err("pane close acknowledgement sequence is reused");
                    }
                }
                (DeliveryState::Closed, None) if !self.is_closed() => {
                    return Err("live generation closed pane lacks close acknowledgement token");
                }
                (DeliveryState::Closed, None) => {}
                (
                    DeliveryState::Clean | DeliveryState::Dirty | DeliveryState::InFlight { .. },
                    Some(_),
                ) => {
                    return Err("non-closed pane retains close acknowledgement token");
                }
                (
                    DeliveryState::Clean | DeliveryState::Dirty | DeliveryState::InFlight { .. },
                    None,
                ) => {}
            }
            if let DeliveryState::InFlight { token, .. } = entry.state {
                observed_inflight_panes += 1;
                if token.instance != self.instance || token.generation != self.generation {
                    return Err("pane token belongs to another generation");
                }
                if !tokens.insert(token) {
                    return Err("delivery token is reused");
                }
            }
        }
        if observed_inflight_panes != self.inflight_panes {
            return Err("in-flight pane count disagrees with pane states");
        }

        if let DeliveryState::InFlight { token, .. } = self.resync_all {
            if token.instance != self.instance || token.generation != self.generation {
                return Err("resync token belongs to another generation");
            }
            if !tokens.insert(token) {
                return Err("delivery token is reused");
            }
            if self.inflight_panes != 0 {
                return Err("resync-all must not overtake an in-flight pane");
            }
        }

        if self.waiter.is_some() && self.pending_wake.is_some() {
            return Err("registered waiter coexists with an undelivered pending wake");
        }
        if self.is_closed()
            && (self
                .panes
                .values()
                .any(|entry| entry.state != DeliveryState::Closed)
                || !self.ready.is_empty()
                || self.inflight_panes != 0
                || self.waiter.is_some())
        {
            return Err("closed generation retains deliverable work");
        }

        Ok(())
    }

    fn issue_token(&mut self) -> Result<DeliveryToken, ClaimError> {
        let Some(sequence) = self.next_token_sequence else {
            DeliveryCounters::increment(&mut self.counters.terminal_failures);
            self.close_generation();
            return Err(ClaimError::TokenExhausted);
        };
        self.next_token_sequence = sequence.checked_add(1);
        Ok(DeliveryToken {
            instance: self.instance,
            generation: self.generation,
            sequence,
        })
    }

    fn issue_pane_close_ack(&mut self, pane_id: PaneId) -> Option<PaneCloseAckToken> {
        let Some(sequence) = self.next_close_ack_sequence else {
            DeliveryCounters::increment(&mut self.counters.terminal_failures);
            self.close_generation();
            return None;
        };
        self.next_close_ack_sequence = sequence.checked_add(1);
        Some(PaneCloseAckToken {
            instance: self.instance,
            generation: self.generation,
            pane_id,
            sequence,
        })
    }

    fn commit_resync_all(&mut self, claim_token: DeliveryToken) -> SettleOutcome {
        let DeliveryState::InFlight { redirtied, token } = self.resync_all else {
            return SettleOutcome::StaleOrDuplicate;
        };
        if token != claim_token {
            return SettleOutcome::StaleOrDuplicate;
        }
        DeliveryCounters::increment(&mut self.counters.commits);
        if redirtied {
            self.resync_all = DeliveryState::Dirty;
            SettleOutcome::RequeuedDirty
        } else {
            self.resync_all = DeliveryState::Clean;
            SettleOutcome::CommittedClean
        }
    }

    fn commit_pane(&mut self, pane_id: PaneId, claim_token: DeliveryToken) -> SettleOutcome {
        let Some(entry) = self.panes.get_mut(&pane_id) else {
            return SettleOutcome::StaleOrDuplicate;
        };
        let DeliveryState::InFlight { redirtied, token } = entry.state else {
            return if entry.state == DeliveryState::Closed {
                SettleOutcome::ScopeClosed
            } else {
                SettleOutcome::StaleOrDuplicate
            };
        };
        if token != claim_token {
            return SettleOutcome::StaleOrDuplicate;
        }

        DeliveryCounters::increment(&mut self.counters.commits);
        self.inflight_panes -= 1;
        if self.resync_all != DeliveryState::Clean {
            entry.state = DeliveryState::Clean;
            entry.queued = false;
            return SettleOutcome::SupersededByResyncAll;
        }
        if redirtied {
            entry.state = DeliveryState::Dirty;
            entry.queued = true;
            self.ready.push_back(pane_id);
            SettleOutcome::RequeuedDirty
        } else {
            entry.state = DeliveryState::Clean;
            entry.queued = false;
            SettleOutcome::CommittedClean
        }
    }

    fn redirty_inflight_resync(&mut self) -> bool {
        if let DeliveryState::InFlight {
            redirtied: false,
            token,
        } = self.resync_all
        {
            self.resync_all = DeliveryState::InFlight {
                redirtied: true,
                token,
            };
            true
        } else {
            false
        }
    }

    fn has_claimable_work(&self) -> bool {
        match self.resync_all {
            DeliveryState::Clean => !self.ready.is_empty(),
            DeliveryState::Dirty => self.inflight_panes == 0,
            DeliveryState::InFlight { .. } | DeliveryState::Closed => false,
        }
    }

    fn wake_if_newly_claimable(&mut self, was_claimable: bool) {
        if !was_claimable && self.has_claimable_work() {
            self.defer_waiter_wake();
        } else if self.waiter.is_some() {
            DeliveryCounters::increment(&mut self.counters.suppressed_wakes);
        }
    }

    fn defer_waiter_wake(&mut self) {
        if let Some(waiter) = self.waiter.take() {
            debug_assert!(
                self.pending_wake.is_none(),
                "the owner must extract each pending wake before another mutation"
            );
            self.pending_wake = Some(waiter);
        }
    }

    #[cfg(test)]
    fn set_next_token_sequence_for_test(&mut self, sequence: Option<u64>) {
        self.next_token_sequence = sequence;
    }

    #[cfg(test)]
    fn set_next_close_ack_sequence_for_test(&mut self, sequence: Option<u64>) {
        self.next_close_ack_sequence = sequence;
    }
}

/// Delivery class for mux notifications.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NotificationClass {
    RenderInvalidation,
    LifecycleBarrier,
    StateInvalidation,
    PayloadEvent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NotificationPriority {
    Barrier,
    InteractiveState,
    Render,
    Advisory,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NotificationOrdering {
    StrictFifo,
    PerKeyCausal,
    GenerationCausal,
    /// The folded telemetry state must be observed before a later pane render
    /// whose production is causally after the telemetry occurrence.
    CausallyBeforePaneRender,
    None,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NotificationFairness {
    /// FIFO within the class, with the cross-class priority burst capped by
    /// [`DELIVERY_PRIORITY_BURST_LIMIT`].
    BoundedPriorityFifo,
    /// Round-robin across dirty panes; redirtied work rejoins at the tail.
    PaneRoundRobin,
    /// Round-robin across stable coalescing keys.
    StableKeyRoundRobin,
    NotApplicable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NotificationCoalescing {
    Forbidden,
    LatestPerStableKey,
    PerPaneDirtyBit,
    /// Multiple synchronized-output occurrences may be folded only when the
    /// fold retains exact counters, maxima, final depth/bytes, and raw causal
    /// order within the accumulator.
    LosslessPerPaneFold,
    /// One publication has both latest-state and noncoalescible event effects.
    StateAndStrictFifoEvent,
    /// A notification represents a derived authoritative level.  Consumers
    /// recheck the level rather than treating the notification as an edge.
    AuthoritativeLevelPredicate,
}

/// Required behavior when the bounded transport queue cannot accept an edge.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FullQueueContract {
    /// State already persisted in a bounded level-triggered store.  The wake
    /// edge may fail, but the consumer must inspect obligations before sleep.
    PreserveDurableObligation,
    /// The mutation is still preventable.  Reserve capacity before mutation,
    /// or durably journal the exact occurrence before returning to the
    /// producer.  The current subscriber-liveness callback cannot by itself
    /// implement producer rejection.
    RequirePreMutationPermitOrJournal,
    /// Authoritative state already changed before notification fanout.  The
    /// occurrence must be journaled and a topology/auxiliary snapshot resync
    /// retained; pretending to reject the producer is impossible.
    JournalThenResyncAuthoritativeState,
    /// Fold the occurrence into a bounded, lossless per-pane accumulator.
    FoldIntoDurableAccumulator,
    /// Reserve both item and byte capacity before accepting the command, or
    /// durably spool it.  An item-count-only queue is not a memory bound.
    RequireByteBudgetPermitOrSpool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClosedQueueContract {
    /// Reject the current producer and terminally close this local generation.
    /// Reconnect must establish a fresh ledger and authoritative bootstrap.
    RejectAndCloseGeneration,
    /// A payload-free wake after shutdown carries no durable information.
    DropBestEffortWake,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NotificationContract {
    pub class: NotificationClass,
    pub priority: NotificationPriority,
    pub ordering: NotificationOrdering,
    pub fairness: NotificationFairness,
    pub coalescing: NotificationCoalescing,
    pub on_full: FullQueueContract,
    pub on_closed: ClosedQueueContract,
}

/// Stable latest-state key retained independently of a lossy wake edge.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum NotificationStateKey<'a> {
    PaneCurrentWorkingDirectory(PaneId),
    PaneEffectiveTitle(PaneId),
    ResolvedWindowTitleForPane(PaneId),
    ResolvedTabTitleForPane(PaneId),
    PanePalette(PaneId),
    PaneUserVar { pane_id: PaneId, name: &'a str },
    PaneUnseenOutput(PaneId),
    PaneProgress(PaneId),
    PaneMouseShape(PaneId),
    WindowLayout(WindowId),
    WindowWorkspace(WindowId),
    ClientActiveWorkspace(&'a ClientId),
    ResolvedWindowFocusForPane(PaneId),
    TabGeometry(TabId),
    TabTitle(TabId),
    WindowTitle(WindowId),
    MuxEmptiness,
}

/// Render invalidation key.  A tab resize invalidates all current member panes
/// and therefore cannot be represented as a single arbitrary pane key.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum NotificationRenderKey {
    Pane(PaneId),
    TabMembers(TabId),
}

/// Topology/lifecycle consequence that an authoritative bootstrap must cover.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum NotificationTopologyEffect<'a> {
    None,
    PaneAdded(PaneId),
    PaneRemovedTombstone(PaneId),
    WindowCreated(WindowId),
    WindowRemoved(WindowId),
    WindowLayout(WindowId),
    WindowWorkspace(WindowId),
    ResolvedWindowTitleForPane(PaneId),
    ResolvedTabTitleForPane(PaneId),
    TabAddedOrMoved {
        tab_id: TabId,
        window_id: WindowId,
    },
    ResolvedWindowFocusForPane(PaneId),
    TabGeometry(TabId),
    TabTitle(TabId),
    WindowTitle(WindowId),
    WorkspaceRename {
        old_workspace: &'a str,
        new_workspace: &'a str,
    },
}

/// State absent from the current render/topology bootstrap and therefore
/// requiring an explicit bounded auxiliary snapshot lane before v1 can claim
/// reconnect convergence.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AuxiliarySnapshotKey<'a> {
    PanePalette(PaneId),
    PaneUserVar { pane_id: PaneId, name: &'a str },
    PaneUnseenOutput(PaneId),
    PaneProgress(PaneId),
    PaneMouseShape(PaneId),
    ClientActiveWorkspace(&'a ClientId),
    ActiveWindowTabForPane(PaneId),
}

/// Noncoalescible user-visible or automation-visible occurrence.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum NotificationEventKey<'a> {
    Bell(PaneId),
    Toast(PaneId),
    PaneUserVar { pane_id: PaneId, name: &'a str },
    ProfileSwitchRequest(PaneId),
    ImageAltText { pane_id: PaneId, image_id: u32 },
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum NotificationEventEffect<'a> {
    None,
    StrictFifo(NotificationEventKey<'a>),
    /// The state lane provides bounded convergence; the event journal
    /// separately preserves every Lua-visible occurrence.
    StateAndStrictFifo(NotificationEventKey<'a>),
    /// Synchronized-output telemetry is foldable, but only by an exact
    /// operation-derived accumulator and before the ensuing pane render.
    LosslessSynchronizedOutputFold {
        pane_id: PaneId,
    },
}

/// Externally visible command that cannot be reconstructed from a render or
/// topology snapshot.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum NotificationCommandEffect {
    None,
    Clipboard { pane_id: PaneId },
    SaveToDownloads { payload_bytes: usize },
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum NotificationLevelPredicate {
    None,
    /// `Empty` means the mux activity level became zero.  Shutdown consumers
    /// must recheck that level; the edge is not a payload-free wake.
    MuxBecameEmpty,
}

/// Where correctness authority lives when ingress is saturated.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NotificationAdmissionContract {
    LevelStoreBeforeWake,
    PreMutationPermitOrJournal,
    PostMutationJournalThenTopologyResync,
    PostMutationJournalThenAuxiliaryResync,
    DurableEventJournalBeforeReturn,
    LosslessFoldBeforeWake,
    ByteBudgetPermitOrDurableSpool,
    RecheckAuthoritativeLevel,
}

/// Multi-axis effects for one mux notification.
///
/// A single exclusive class is insufficient: palette is state plus render,
/// `SetUserVar` is state plus an exact event, and bell is an exact event plus
/// a transient render effect.  The fixed two-key array is sufficient for the
/// v1 inventory and makes the memory bound explicit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NotificationEffects<'a> {
    pub primary: NotificationContract,
    pub state_keys: [Option<NotificationStateKey<'a>>; 2],
    pub render_key: Option<NotificationRenderKey>,
    pub topology: NotificationTopologyEffect<'a>,
    pub auxiliary_snapshot: Option<AuxiliarySnapshotKey<'a>>,
    pub event: NotificationEventEffect<'a>,
    pub command: NotificationCommandEffect,
    pub level_predicate: NotificationLevelPredicate,
    pub admission: NotificationAdmissionContract,
}

fn contract(
    class: NotificationClass,
    priority: NotificationPriority,
    ordering: NotificationOrdering,
    fairness: NotificationFairness,
    coalescing: NotificationCoalescing,
    on_full: FullQueueContract,
) -> NotificationContract {
    NotificationContract {
        class,
        priority,
        ordering,
        fairness,
        coalescing,
        on_full,
        on_closed: ClosedQueueContract::RejectAndCloseGeneration,
    }
}

fn state_contract() -> NotificationContract {
    contract(
        NotificationClass::StateInvalidation,
        NotificationPriority::InteractiveState,
        NotificationOrdering::PerKeyCausal,
        NotificationFairness::StableKeyRoundRobin,
        NotificationCoalescing::LatestPerStableKey,
        FullQueueContract::PreserveDurableObligation,
    )
}

fn lifecycle_contract(on_full: FullQueueContract) -> NotificationContract {
    contract(
        NotificationClass::LifecycleBarrier,
        NotificationPriority::Barrier,
        NotificationOrdering::StrictFifo,
        NotificationFairness::BoundedPriorityFifo,
        NotificationCoalescing::Forbidden,
        on_full,
    )
}

fn payload_contract(
    coalescing: NotificationCoalescing,
    ordering: NotificationOrdering,
    on_full: FullQueueContract,
) -> NotificationContract {
    contract(
        NotificationClass::PayloadEvent,
        NotificationPriority::Advisory,
        ordering,
        NotificationFairness::BoundedPriorityFifo,
        coalescing,
        on_full,
    )
}

fn effects(primary: NotificationContract) -> NotificationEffects<'static> {
    NotificationEffects {
        primary,
        state_keys: [None, None],
        render_key: None,
        topology: NotificationTopologyEffect::None,
        auxiliary_snapshot: None,
        event: NotificationEventEffect::None,
        command: NotificationCommandEffect::None,
        level_predicate: NotificationLevelPredicate::None,
        admission: NotificationAdmissionContract::LevelStoreBeforeWake,
    }
}

fn alert_effects(pane_id: PaneId, alert: &wezterm_term::Alert) -> NotificationEffects<'_> {
    use wezterm_term::Alert;

    match alert {
        Alert::Bell => NotificationEffects {
            render_key: Some(NotificationRenderKey::Pane(pane_id)),
            event: NotificationEventEffect::StrictFifo(NotificationEventKey::Bell(pane_id)),
            admission: NotificationAdmissionContract::DurableEventJournalBeforeReturn,
            ..effects(payload_contract(
                NotificationCoalescing::Forbidden,
                NotificationOrdering::GenerationCausal,
                FullQueueContract::RequirePreMutationPermitOrJournal,
            ))
        },
        Alert::ToastNotification { .. } => NotificationEffects {
            event: NotificationEventEffect::StrictFifo(NotificationEventKey::Toast(pane_id)),
            admission: NotificationAdmissionContract::DurableEventJournalBeforeReturn,
            ..effects(payload_contract(
                NotificationCoalescing::Forbidden,
                NotificationOrdering::GenerationCausal,
                FullQueueContract::RequirePreMutationPermitOrJournal,
            ))
        },
        Alert::CurrentWorkingDirectoryChanged => NotificationEffects {
            state_keys: [
                Some(NotificationStateKey::PaneCurrentWorkingDirectory(pane_id)),
                None,
            ],
            render_key: Some(NotificationRenderKey::Pane(pane_id)),
            ..effects(state_contract())
        },
        Alert::IconTitleChanged(_) => NotificationEffects {
            state_keys: [
                Some(NotificationStateKey::PaneEffectiveTitle(pane_id)),
                None,
            ],
            render_key: Some(NotificationRenderKey::Pane(pane_id)),
            ..effects(state_contract())
        },
        Alert::WindowTitleChanged(_) => NotificationEffects {
            state_keys: [
                Some(NotificationStateKey::PaneEffectiveTitle(pane_id)),
                Some(NotificationStateKey::ResolvedWindowTitleForPane(pane_id)),
            ],
            render_key: Some(NotificationRenderKey::Pane(pane_id)),
            topology: NotificationTopologyEffect::ResolvedWindowTitleForPane(pane_id),
            ..effects(state_contract())
        },
        Alert::TabTitleChanged(_) => NotificationEffects {
            state_keys: [
                Some(NotificationStateKey::PaneEffectiveTitle(pane_id)),
                Some(NotificationStateKey::ResolvedTabTitleForPane(pane_id)),
            ],
            render_key: Some(NotificationRenderKey::Pane(pane_id)),
            topology: NotificationTopologyEffect::ResolvedTabTitleForPane(pane_id),
            ..effects(state_contract())
        },
        Alert::PaletteChanged => NotificationEffects {
            state_keys: [Some(NotificationStateKey::PanePalette(pane_id)), None],
            render_key: Some(NotificationRenderKey::Pane(pane_id)),
            auxiliary_snapshot: Some(AuxiliarySnapshotKey::PanePalette(pane_id)),
            admission: NotificationAdmissionContract::PostMutationJournalThenAuxiliaryResync,
            ..effects(state_contract())
        },
        Alert::SetUserVar { name, .. } => NotificationEffects {
            primary: payload_contract(
                NotificationCoalescing::StateAndStrictFifoEvent,
                NotificationOrdering::GenerationCausal,
                FullQueueContract::JournalThenResyncAuthoritativeState,
            ),
            state_keys: [
                Some(NotificationStateKey::PaneUserVar { pane_id, name }),
                None,
            ],
            auxiliary_snapshot: Some(AuxiliarySnapshotKey::PaneUserVar { pane_id, name }),
            event: NotificationEventEffect::StateAndStrictFifo(NotificationEventKey::PaneUserVar {
                pane_id,
                name,
            }),
            admission: NotificationAdmissionContract::PostMutationJournalThenAuxiliaryResync,
            ..effects(state_contract())
        },
        Alert::OutputSinceFocusLost => NotificationEffects {
            state_keys: [Some(NotificationStateKey::PaneUnseenOutput(pane_id)), None],
            auxiliary_snapshot: Some(AuxiliarySnapshotKey::PaneUnseenOutput(pane_id)),
            admission: NotificationAdmissionContract::PostMutationJournalThenAuxiliaryResync,
            ..effects(state_contract())
        },
        Alert::Progress(_) => NotificationEffects {
            state_keys: [Some(NotificationStateKey::PaneProgress(pane_id)), None],
            auxiliary_snapshot: Some(AuxiliarySnapshotKey::PaneProgress(pane_id)),
            admission: NotificationAdmissionContract::PostMutationJournalThenAuxiliaryResync,
            ..effects(state_contract())
        },
        Alert::SetProfileRequested { .. } => NotificationEffects {
            event: NotificationEventEffect::StrictFifo(NotificationEventKey::ProfileSwitchRequest(
                pane_id,
            )),
            admission: NotificationAdmissionContract::DurableEventJournalBeforeReturn,
            ..effects(payload_contract(
                NotificationCoalescing::Forbidden,
                NotificationOrdering::GenerationCausal,
                FullQueueContract::RequirePreMutationPermitOrJournal,
            ))
        },
        Alert::MouseShapeRequested { .. } => NotificationEffects {
            state_keys: [Some(NotificationStateKey::PaneMouseShape(pane_id)), None],
            auxiliary_snapshot: Some(AuxiliarySnapshotKey::PaneMouseShape(pane_id)),
            admission: NotificationAdmissionContract::PostMutationJournalThenAuxiliaryResync,
            ..effects(state_contract())
        },
        Alert::ImageAltText { image_id, .. } => NotificationEffects {
            event: NotificationEventEffect::StrictFifo(NotificationEventKey::ImageAltText {
                pane_id,
                image_id: *image_id,
            }),
            admission: NotificationAdmissionContract::DurableEventJournalBeforeReturn,
            ..effects(payload_contract(
                NotificationCoalescing::Forbidden,
                NotificationOrdering::GenerationCausal,
                FullQueueContract::RequirePreMutationPermitOrJournal,
            ))
        },
    }
}

/// Return the complete v1 delivery-effect inventory for a mux notification.
///
/// Priority is a scheduling preference, not permission to starve.  Durable
/// classes must be serviced with bounded bursts across priority bands;
/// per-pane render invalidations use the ledger's FIFO round-robin order.
#[must_use]
pub fn notification_effects(notification: &MuxNotification) -> NotificationEffects<'_> {
    match notification {
        MuxNotification::PaneOutput(pane_id) => NotificationEffects {
            render_key: Some(NotificationRenderKey::Pane(*pane_id)),
            ..effects(contract(
                NotificationClass::RenderInvalidation,
                NotificationPriority::Render,
                NotificationOrdering::PerKeyCausal,
                NotificationFairness::PaneRoundRobin,
                NotificationCoalescing::PerPaneDirtyBit,
                FullQueueContract::PreserveDurableObligation,
            ))
        },
        MuxNotification::SynchronizedOutput { pane_id, .. } => NotificationEffects {
            event: NotificationEventEffect::LosslessSynchronizedOutputFold { pane_id: *pane_id },
            admission: NotificationAdmissionContract::LosslessFoldBeforeWake,
            ..effects(payload_contract(
                NotificationCoalescing::LosslessPerPaneFold,
                NotificationOrdering::CausallyBeforePaneRender,
                FullQueueContract::FoldIntoDurableAccumulator,
            ))
        },
        MuxNotification::PaneAdded(pane_id) => NotificationEffects {
            topology: NotificationTopologyEffect::PaneAdded(*pane_id),
            admission: NotificationAdmissionContract::PostMutationJournalThenTopologyResync,
            ..effects(lifecycle_contract(
                FullQueueContract::JournalThenResyncAuthoritativeState,
            ))
        },
        MuxNotification::PaneRemoved(pane_id) => NotificationEffects {
            topology: NotificationTopologyEffect::PaneRemovedTombstone(*pane_id),
            admission: NotificationAdmissionContract::PostMutationJournalThenTopologyResync,
            ..effects(lifecycle_contract(
                FullQueueContract::JournalThenResyncAuthoritativeState,
            ))
        },
        MuxNotification::WindowCreated(window_id) => NotificationEffects {
            topology: NotificationTopologyEffect::WindowCreated(*window_id),
            admission: NotificationAdmissionContract::PostMutationJournalThenTopologyResync,
            ..effects(lifecycle_contract(
                FullQueueContract::JournalThenResyncAuthoritativeState,
            ))
        },
        MuxNotification::WindowRemoved(window_id) => NotificationEffects {
            topology: NotificationTopologyEffect::WindowRemoved(*window_id),
            admission: NotificationAdmissionContract::PostMutationJournalThenTopologyResync,
            ..effects(lifecycle_contract(
                FullQueueContract::JournalThenResyncAuthoritativeState,
            ))
        },
        MuxNotification::WindowInvalidated(window_id) => NotificationEffects {
            state_keys: [Some(NotificationStateKey::WindowLayout(*window_id)), None],
            topology: NotificationTopologyEffect::WindowLayout(*window_id),
            ..effects(state_contract())
        },
        MuxNotification::WindowWorkspaceChanged(window_id) => NotificationEffects {
            state_keys: [
                Some(NotificationStateKey::WindowWorkspace(*window_id)),
                None,
            ],
            topology: NotificationTopologyEffect::WindowWorkspace(*window_id),
            ..effects(state_contract())
        },
        MuxNotification::ActiveWorkspaceChanged(client_id) => NotificationEffects {
            state_keys: [
                Some(NotificationStateKey::ClientActiveWorkspace(
                    client_id.as_ref(),
                )),
                None,
            ],
            auxiliary_snapshot: Some(AuxiliarySnapshotKey::ClientActiveWorkspace(
                client_id.as_ref(),
            )),
            admission: NotificationAdmissionContract::PostMutationJournalThenAuxiliaryResync,
            ..effects(state_contract())
        },
        MuxNotification::Alert { pane_id, alert } => alert_effects(*pane_id, alert),
        MuxNotification::Empty => NotificationEffects {
            primary: contract(
                NotificationClass::LifecycleBarrier,
                NotificationPriority::Barrier,
                NotificationOrdering::PerKeyCausal,
                NotificationFairness::StableKeyRoundRobin,
                NotificationCoalescing::AuthoritativeLevelPredicate,
                FullQueueContract::PreserveDurableObligation,
            ),
            state_keys: [Some(NotificationStateKey::MuxEmptiness), None],
            level_predicate: NotificationLevelPredicate::MuxBecameEmpty,
            admission: NotificationAdmissionContract::RecheckAuthoritativeLevel,
            ..effects(state_contract())
        },
        MuxNotification::AssignClipboard { pane_id, .. } => NotificationEffects {
            command: NotificationCommandEffect::Clipboard { pane_id: *pane_id },
            admission: NotificationAdmissionContract::PreMutationPermitOrJournal,
            ..effects(payload_contract(
                NotificationCoalescing::Forbidden,
                NotificationOrdering::StrictFifo,
                FullQueueContract::RequirePreMutationPermitOrJournal,
            ))
        },
        MuxNotification::SaveToDownloads { data, .. } => NotificationEffects {
            command: NotificationCommandEffect::SaveToDownloads {
                payload_bytes: data.len(),
            },
            admission: NotificationAdmissionContract::ByteBudgetPermitOrDurableSpool,
            ..effects(payload_contract(
                NotificationCoalescing::Forbidden,
                NotificationOrdering::StrictFifo,
                FullQueueContract::RequireByteBudgetPermitOrSpool,
            ))
        },
        MuxNotification::TabAddedToWindow { tab_id, window_id } => NotificationEffects {
            topology: NotificationTopologyEffect::TabAddedOrMoved {
                tab_id: *tab_id,
                window_id: *window_id,
            },
            admission: NotificationAdmissionContract::PostMutationJournalThenTopologyResync,
            ..effects(lifecycle_contract(
                FullQueueContract::JournalThenResyncAuthoritativeState,
            ))
        },
        MuxNotification::PaneFocused(pane_id) => NotificationEffects {
            state_keys: [
                Some(NotificationStateKey::ResolvedWindowFocusForPane(*pane_id)),
                None,
            ],
            topology: NotificationTopologyEffect::ResolvedWindowFocusForPane(*pane_id),
            auxiliary_snapshot: Some(AuxiliarySnapshotKey::ActiveWindowTabForPane(*pane_id)),
            admission: NotificationAdmissionContract::PostMutationJournalThenAuxiliaryResync,
            ..effects(state_contract())
        },
        MuxNotification::TabResized(tab_id) => NotificationEffects {
            state_keys: [Some(NotificationStateKey::TabGeometry(*tab_id)), None],
            render_key: Some(NotificationRenderKey::TabMembers(*tab_id)),
            topology: NotificationTopologyEffect::TabGeometry(*tab_id),
            ..effects(state_contract())
        },
        MuxNotification::TabTitleChanged { tab_id, .. } => NotificationEffects {
            state_keys: [Some(NotificationStateKey::TabTitle(*tab_id)), None],
            topology: NotificationTopologyEffect::TabTitle(*tab_id),
            ..effects(state_contract())
        },
        MuxNotification::WindowTitleChanged { window_id, .. } => NotificationEffects {
            state_keys: [Some(NotificationStateKey::WindowTitle(*window_id)), None],
            topology: NotificationTopologyEffect::WindowTitle(*window_id),
            ..effects(state_contract())
        },
        MuxNotification::WorkspaceRenamed {
            old_workspace,
            new_workspace,
        } => NotificationEffects {
            topology: NotificationTopologyEffect::WorkspaceRename {
                old_workspace,
                new_workspace,
            },
            admission: NotificationAdmissionContract::PreMutationPermitOrJournal,
            ..effects(lifecycle_contract(
                FullQueueContract::RequirePreMutationPermitOrJournal,
            ))
        },
    }
}

/// Return the primary scheduling projection of [`notification_effects`].
///
/// Consumers that need correctness semantics must use the full multi-axis
/// inventory; this projection exists only for scheduler lane selection.
#[must_use]
pub fn notification_contract(notification: &MuxNotification) -> NotificationContract {
    notification_effects(notification).primary
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Wake;
    use wezterm_term::Alert;

    #[derive(Default)]
    struct WakeCounter(AtomicUsize);

    impl Wake for WakeCounter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn generation() -> DeliveryGeneration {
        DeliveryGeneration::new(17)
    }

    fn ledger_for(generation: DeliveryGeneration, pane_limit: usize) -> DeliveryLedger {
        DeliveryLedger::new(generation, pane_limit)
            .expect("test delivery-ledger instance allocation should succeed")
    }

    fn ledger(pane_limit: usize) -> DeliveryLedger {
        ledger_for(generation(), pane_limit)
    }

    fn claim(ledger: &mut DeliveryLedger) -> DeliveryClaim {
        ledger
            .claim_next()
            .expect("token allocation should succeed")
            .expect("expected a delivery claim")
    }

    fn mutate_ledger_under_owner<T>(
        ledger: &mut DeliveryLedger,
        mutation: impl FnOnce(&mut DeliveryLedger) -> T,
    ) -> (T, Option<PendingDeliveryWake>) {
        // This exclusive borrow models the live external mutex guard. Returning
        // the pending wake ends that guard boundary; tests must wake afterward.
        let outcome = mutation(ledger);
        let pending_wake = ledger.take_pending_wake();
        (outcome, pending_wake)
    }

    fn close_ack(outcome: ClosePaneOutcome) -> PaneCloseAckToken {
        match outcome {
            ClosePaneOutcome::ClosedClean { close_ack }
            | ClosePaneOutcome::ClosedDirty { close_ack }
            | ClosePaneOutcome::ClosedInFlight { close_ack, .. }
            | ClosePaneOutcome::AlreadyClosed { close_ack } => close_ack,
            ClosePaneOutcome::Untracked | ClosePaneOutcome::GenerationClosed => {
                panic!("expected a tracked pane close with acknowledgement token")
            }
        }
    }

    #[test]
    fn ledger_instance_allocator_reserves_zero_and_terminal_value() {
        let zero = AtomicU64::new(0);
        assert_eq!(
            allocate_delivery_ledger_instance(&zero),
            Err(DeliveryLedgerInstanceExhausted)
        );

        let terminal_boundary = AtomicU64::new(u64::MAX - 1);
        assert_eq!(
            allocate_delivery_ledger_instance(&terminal_boundary),
            Ok(DeliveryLedgerInstance(u64::MAX - 1))
        );
        assert_eq!(terminal_boundary.load(Ordering::Relaxed), u64::MAX);
        assert_eq!(
            allocate_delivery_ledger_instance(&terminal_boundary),
            Err(DeliveryLedgerInstanceExhausted)
        );
    }

    #[test]
    fn dirty_redirty_commit_cycle_is_level_triggered() {
        let mut ledger = ledger(4);
        assert_eq!(ledger.mark_dirty(7), DirtyOutcome::BecameDirty);
        assert_eq!(ledger.mark_dirty(7), DirtyOutcome::Coalesced);

        let first = claim(&mut ledger);
        assert_eq!(first.scope(), DeliveryScope::Pane(7));
        assert_eq!(ledger.mark_dirty(7), DirtyOutcome::MarkedInFlightRedirty);
        assert_eq!(ledger.commit(first), SettleOutcome::RequeuedDirty);

        let second = claim(&mut ledger);
        assert_ne!(first.token(), second.token());
        assert_eq!(ledger.commit(second), SettleOutcome::CommittedClean);
        assert_eq!(ledger.pane_state(7), Some(DeliveryState::Clean));
        assert_eq!(
            ledger.counters(),
            DeliveryCounters {
                dirties: 3,
                coalesces: 1,
                commits: 2,
                ..DeliveryCounters::default()
            }
        );
        assert_eq!(ledger.check_invariants(), Ok(()));
    }

    #[test]
    fn only_wake_full_then_producer_quiesces_but_obligation_survives() {
        let mut ledger = ledger(1);
        let mut bounded_wakes = VecDeque::from(["older-dispatch-item"]);
        let wake_capacity = 1;

        assert_eq!(ledger.mark_dirty(9), DirtyOutcome::BecameDirty);
        let wake_admitted = if bounded_wakes.len() < wake_capacity {
            bounded_wakes.push_back("render-wake");
            true
        } else {
            false
        };
        assert!(!wake_admitted, "the only render wake must observe Full");

        // The producer is now permanently quiescent.  Consuming the item that
        // made the queue full creates the mandatory before-sleep inspection
        // boundary; no later pane event and no timer are involved.
        assert_eq!(bounded_wakes.pop_front(), Some("older-dispatch-item"));
        let durable = claim(&mut ledger);
        assert_eq!(durable.scope(), DeliveryScope::Pane(9));
        assert_eq!(ledger.commit(durable), SettleOutcome::CommittedClean);
        assert!(bounded_wakes.is_empty());
        assert_eq!(ledger.check_invariants(), Ok(()));
    }

    #[test]
    fn atomic_wait_registration_closes_the_check_then_park_race() {
        let mut ledger = ledger(1);
        let wake_counter = Arc::new(WakeCounter::default());
        let waker = Waker::from(Arc::clone(&wake_counter));
        let mut context = Context::from_waker(&waker);

        // The consumer performs its empty check and atomically registers the
        // task that would otherwise be about to park.
        assert_eq!(ledger.poll_claim_next(&mut context), Poll::Pending);
        assert_eq!(
            ledger.claim_next(),
            Ok(None),
            "an empty direct probe must not erase a registered waiter"
        );

        // The producer runs strictly after that check, has no queue capacity,
        // publishes the only dirty, and then permanently quiesces.
        let (outcome, pending_wake) =
            mutate_ledger_under_owner(&mut ledger, |owned| owned.mark_dirty(3));
        assert_eq!(outcome, DirtyOutcome::BecameDirty);
        assert_eq!(
            wake_counter.0.load(Ordering::Relaxed),
            0,
            "owner mutation must not invoke an arbitrary waker under its guard"
        );
        pending_wake
            .expect("claimable publication must defer the registered wake")
            .wake();
        assert_eq!(
            wake_counter.0.load(Ordering::Relaxed),
            1,
            "publishing after the empty check must wake the registered consumer"
        );

        assert!(matches!(
            ledger.poll_claim_next(&mut context),
            Poll::Ready(Ok(Some(DeliveryClaim {
                scope: DeliveryScope::Pane(3),
                ..
            })))
        ));

        // The opposite ordering is also safe: preexisting work is returned
        // synchronously and no waiter is installed.
        let mut producer_first = self::ledger(1);
        producer_first.mark_dirty(4);
        assert!(matches!(
            producer_first.poll_claim_next(&mut context),
            Poll::Ready(Ok(Some(DeliveryClaim {
                scope: DeliveryScope::Pane(4),
                ..
            })))
        ));
        assert_eq!(
            wake_counter.0.load(Ordering::Relaxed),
            1,
            "ready work must not schedule a redundant wake"
        );
    }

    #[test]
    fn terminal_close_defers_waiter_until_after_owner_release() {
        let mut ledger = ledger(1);
        let wake_counter = Arc::new(WakeCounter::default());
        let waker = Waker::from(Arc::clone(&wake_counter));
        let mut context = Context::from_waker(&waker);
        assert_eq!(ledger.poll_claim_next(&mut context), Poll::Pending);

        let ((), pending_wake) =
            mutate_ledger_under_owner(&mut ledger, DeliveryLedger::close_generation);
        assert_eq!(
            wake_counter.0.load(Ordering::Relaxed),
            0,
            "terminal close must not invoke the waker under owner ownership"
        );
        assert_eq!(ledger.check_invariants(), Ok(()));
        pending_wake
            .expect("terminal close must extract the registered waiter")
            .wake();
        assert_eq!(wake_counter.0.load(Ordering::Relaxed), 1);
        assert_eq!(ledger.counters().wake_deliveries, 1);
    }

    #[test]
    fn in_flight_redirty_wake_storm_is_suppressed_until_settlement() {
        let mut ledger = ledger(1);
        assert_eq!(ledger.mark_dirty(3), DirtyOutcome::BecameDirty);
        let in_flight = claim(&mut ledger);

        let wake_counter = Arc::new(WakeCounter::default());
        let waker = Waker::from(Arc::clone(&wake_counter));
        let mut context = Context::from_waker(&waker);
        assert_eq!(ledger.poll_claim_next(&mut context), Poll::Pending);

        assert_eq!(ledger.mark_dirty(3), DirtyOutcome::MarkedInFlightRedirty);
        for _ in 1..64 {
            assert_eq!(ledger.mark_dirty(3), DirtyOutcome::Coalesced);
        }
        assert_eq!(
            wake_counter.0.load(Ordering::Relaxed),
            0,
            "redirties fenced by the older claim must retain, not wake, the waiter"
        );
        assert_eq!(ledger.counters().suppressed_wakes, 64);
        assert_eq!(ledger.counters().wake_deliveries, 0);

        let (outcome, pending_wake) =
            mutate_ledger_under_owner(&mut ledger, |owned| owned.commit(in_flight));
        assert_eq!(outcome, SettleOutcome::RequeuedDirty);
        assert_eq!(
            wake_counter.0.load(Ordering::Relaxed),
            0,
            "settlement must only extract the wake while the owner is held"
        );
        pending_wake
            .expect("claimable settlement must defer the registered wake")
            .wake();
        assert_eq!(
            wake_counter.0.load(Ordering::Relaxed),
            1,
            "settlement that exposes the redirtied obligation must wake exactly once"
        );
        assert_eq!(ledger.counters().wake_deliveries, 1);
        assert!(matches!(
            ledger.poll_claim_next(&mut context),
            Poll::Ready(Ok(Some(DeliveryClaim {
                scope: DeliveryScope::Pane(3),
                ..
            })))
        ));
    }

    #[test]
    fn clean_settlement_retains_waiter_until_new_work_becomes_claimable() {
        let mut ledger = ledger(2);
        assert_eq!(ledger.mark_dirty(1), DirtyOutcome::BecameDirty);
        let in_flight = claim(&mut ledger);

        let wake_counter = Arc::new(WakeCounter::default());
        let waker = Waker::from(Arc::clone(&wake_counter));
        let mut context = Context::from_waker(&waker);
        assert_eq!(ledger.poll_claim_next(&mut context), Poll::Pending);

        let (outcome, pending_wake) =
            mutate_ledger_under_owner(&mut ledger, |owned| owned.commit(in_flight));
        assert_eq!(outcome, SettleOutcome::CommittedClean);
        assert!(
            pending_wake.is_none(),
            "a clean settlement must retain rather than extract its waiter"
        );
        assert_eq!(
            wake_counter.0.load(Ordering::Relaxed),
            0,
            "a clean settlement exposes no work and must not spuriously wake"
        );
        assert_eq!(ledger.counters().suppressed_wakes, 1);

        let (outcome, pending_wake) =
            mutate_ledger_under_owner(&mut ledger, |owned| owned.mark_dirty(2));
        assert_eq!(outcome, DirtyOutcome::BecameDirty);
        assert_eq!(
            wake_counter.0.load(Ordering::Relaxed),
            0,
            "claimable publication must not wake until the owner guard is gone"
        );
        pending_wake
            .expect("later claimable dirty must defer the retained waiter")
            .wake();
        assert_eq!(
            wake_counter.0.load(Ordering::Relaxed),
            1,
            "the retained waiter must wake when a later dirty becomes claimable"
        );
        assert_eq!(ledger.counters().wake_deliveries, 1);
    }

    #[test]
    fn close_wakes_resync_blocked_only_by_inflight_pane() {
        let mut ledger = ledger(1);
        assert_eq!(ledger.mark_dirty(1), DirtyOutcome::BecameDirty);
        let pane_claim = claim(&mut ledger);
        assert_eq!(ledger.mark_dirty(2), DirtyOutcome::EscalatedToResyncAll);

        let wake_counter = Arc::new(WakeCounter::default());
        let waker = Waker::from(Arc::clone(&wake_counter));
        let mut context = Context::from_waker(&waker);
        assert_eq!(
            ledger.poll_claim_next(&mut context),
            Poll::Pending,
            "older in-flight pane must fence the ready resync"
        );

        assert_eq!(ledger.mark_dirty(3), DirtyOutcome::CoveredByResyncAll);
        assert_eq!(
            wake_counter.0.load(Ordering::Relaxed),
            0,
            "a dirty already covered by the fenced resync must not wake"
        );
        assert_eq!(ledger.counters().suppressed_wakes, 1);

        let (outcome, pending_wake) =
            mutate_ledger_under_owner(&mut ledger, |owned| owned.close_pane(1));
        assert!(matches!(
            outcome,
            ClosePaneOutcome::ClosedInFlight {
                delivery_token,
                ..
            } if delivery_token == pane_claim.token()
        ));
        assert_eq!(
            ledger.counters().suppressed_wakes,
            1,
            "the fence-clearing close must deliver rather than suppress its wake"
        );
        assert_eq!(
            wake_counter.0.load(Ordering::Relaxed),
            0,
            "fence-clearing close must defer the wake under owner ownership"
        );
        pending_wake
            .expect("fence-clearing close must extract the registered waiter")
            .wake();
        assert_eq!(
            wake_counter.0.load(Ordering::Relaxed),
            1,
            "close removed the last resync fence and must wake the waiter"
        );
        assert!(matches!(
            ledger.poll_claim_next(&mut context),
            Poll::Ready(Ok(Some(DeliveryClaim {
                scope: DeliveryScope::ResyncAll,
                ..
            })))
        ));
    }

    #[test]
    fn zero_and_one_pane_capacity_use_bounded_resync_escape_hatch() {
        let mut zero = ledger(0);
        assert_eq!(zero.mark_dirty(1), DirtyOutcome::EscalatedToResyncAll);
        assert_eq!(
            zero.capacity(),
            DeliveryCapacity {
                pane_limit: 0,
                tracked_panes: 0,
                ready_panes: 0,
                generation_slots: 1,
            }
        );
        let zero_claim = claim(&mut zero);
        assert_eq!(zero_claim.scope(), DeliveryScope::ResyncAll);
        assert_eq!(zero.commit(zero_claim), SettleOutcome::CommittedClean);

        let mut one = ledger(1);
        assert_eq!(one.mark_dirty(1), DirtyOutcome::BecameDirty);
        assert_eq!(one.mark_dirty(2), DirtyOutcome::EscalatedToResyncAll);
        assert_eq!(one.pane_state(1), Some(DeliveryState::Clean));
        assert_eq!(one.capacity().tracked_panes, 1);
        assert_eq!(one.capacity().ready_panes, 0);
        assert_eq!(one.counters().overflows, 1);
        assert_eq!(one.counters().resyncs, 1);
        assert_eq!(claim(&mut one).scope(), DeliveryScope::ResyncAll);
        assert_eq!(one.check_invariants(), Ok(()));
    }

    #[test]
    fn resync_waits_for_older_inflight_and_absorbs_its_redirty() {
        let mut ledger = ledger(2);
        ledger.mark_dirty(1);
        let pane_claim = claim(&mut ledger);
        ledger.mark_dirty(1);
        assert_eq!(ledger.request_resync_all(), ResyncOutcome::Requested);
        assert_eq!(
            ledger.claim_next(),
            Ok(None),
            "resync must not overtake an older in-flight pane"
        );
        assert_eq!(
            ledger.commit(pane_claim),
            SettleOutcome::SupersededByResyncAll
        );
        assert_eq!(claim(&mut ledger).scope(), DeliveryScope::ResyncAll);
        assert_eq!(ledger.check_invariants(), Ok(()));
    }

    #[test]
    fn redirty_during_resync_requires_a_fresh_resync_token() {
        let mut ledger = ledger(0);
        ledger.mark_dirty(1);
        let first = claim(&mut ledger);
        assert_eq!(ledger.mark_dirty(2), DirtyOutcome::MarkedInFlightRedirty);
        assert_eq!(ledger.commit(first), SettleOutcome::RequeuedDirty);

        let second = claim(&mut ledger);
        assert_eq!(second.scope(), DeliveryScope::ResyncAll);
        assert_ne!(first.token(), second.token());
        assert_eq!(ledger.commit(second), SettleOutcome::CommittedClean);
        assert_eq!(ledger.resync_all_state(), DeliveryState::Clean);
    }

    #[test]
    fn round_robin_fairness_appends_redirtied_work_at_tail() {
        let mut ledger = ledger(3);
        for pane_id in 1..=3 {
            ledger.mark_dirty(pane_id);
        }
        let first = claim(&mut ledger);
        assert_eq!(first.scope(), DeliveryScope::Pane(1));
        ledger.mark_dirty(1);
        assert_eq!(ledger.commit(first), SettleOutcome::RequeuedDirty);

        let second = claim(&mut ledger);
        let third = claim(&mut ledger);
        assert_eq!(second.scope(), DeliveryScope::Pane(2));
        assert_eq!(third.scope(), DeliveryScope::Pane(3));
        assert_eq!(ledger.commit(second), SettleOutcome::CommittedClean);
        assert_eq!(ledger.commit(third), SettleOutcome::CommittedClean);
        assert_eq!(claim(&mut ledger).scope(), DeliveryScope::Pane(1));
        assert_eq!(ledger.check_invariants(), Ok(()));
    }

    #[test]
    fn close_while_dirty_or_inflight_is_terminal_for_that_pane() {
        let mut dirty = ledger(1);
        dirty.mark_dirty(4);
        assert!(matches!(
            dirty.close_pane(4),
            ClosePaneOutcome::ClosedDirty { .. }
        ));
        assert_eq!(dirty.claim_next(), Ok(None));
        assert_eq!(dirty.mark_dirty(4), DirtyOutcome::IgnoredClosed);

        let mut inflight = ledger(1);
        inflight.mark_dirty(4);
        let old = claim(&mut inflight);
        assert!(matches!(
            inflight.close_pane(4),
            ClosePaneOutcome::ClosedInFlight {
                delivery_token,
                ..
            } if delivery_token == old.token()
        ));
        assert_eq!(inflight.commit(old), SettleOutcome::ScopeClosed);
        assert_eq!(inflight.retry(old), SettleOutcome::ScopeClosed);
        assert_eq!(inflight.check_invariants(), Ok(()));
    }

    #[test]
    fn acknowledged_close_reclaims_capacity_for_later_allocator_ids() {
        let mut ledger = ledger(1);
        ledger.mark_dirty(1);
        assert!(matches!(
            ledger.close_pane(1),
            ClosePaneOutcome::ClosedDirty { .. }
        ));
        assert_eq!(
            ledger.mark_dirty(2),
            DirtyOutcome::EscalatedToResyncAll,
            "unacknowledged close barrier must retain its bounded tombstone"
        );

        let mut reclaimed = self::ledger(2);
        reclaimed.mark_dirty(1);
        reclaimed.mark_dirty(2);
        let close_ack = close_ack(reclaimed.close_pane(1));
        assert_eq!(
            reclaimed.acknowledge_pane_close(1, close_ack),
            ReclaimPaneOutcome::Reclaimed
        );
        assert_eq!(
            reclaimed.mark_dirty(3),
            DirtyOutcome::BecameDirty,
            "a higher monotonic PaneId must reuse the reclaimed capacity"
        );
        assert_eq!(reclaimed.capacity().tracked_panes, 2);
        assert_eq!(reclaimed.counters().close_acknowledgements, 1);
        assert_eq!(reclaimed.check_invariants(), Ok(()));
    }

    #[test]
    fn repeated_close_returns_same_pending_ack_and_can_reclaim() {
        let mut ledger = ledger(1);
        ledger.mark_dirty(7);
        let first_ack = close_ack(ledger.close_pane(7));
        let repeated_ack = match ledger.close_pane(7) {
            ClosePaneOutcome::AlreadyClosed { close_ack } => close_ack,
            other => panic!("expected recoverable repeated close, got {other:?}"),
        };
        assert_eq!(
            repeated_ack, first_ack,
            "a retry before application ACK must recover the exact pending capability"
        );
        assert_eq!(
            ledger.acknowledge_pane_close(7, repeated_ack),
            ReclaimPaneOutcome::Reclaimed
        );
        assert_eq!(ledger.mark_dirty(8), DirtyOutcome::BecameDirty);
        assert_eq!(ledger.check_invariants(), Ok(()));
    }

    #[test]
    fn higher_reclaimed_id_does_not_retire_a_lower_live_pane_first_observed_late() {
        let mut ledger = ledger(2);
        ledger.mark_dirty(100);
        let close_ack = close_ack(ledger.close_pane(100));
        assert_eq!(
            ledger.acknowledge_pane_close(100, close_ack),
            ReclaimPaneOutcome::Reclaimed
        );

        assert_eq!(
            ledger.mark_dirty(1),
            DirtyOutcome::BecameDirty,
            "numeric order is not liveness evidence; pane 1 may have remained live throughout"
        );
        assert!(!ledger.is_closed());
        assert_eq!(claim(&mut ledger).scope(), DeliveryScope::Pane(1));
        assert_eq!(ledger.check_invariants(), Ok(()));
    }

    #[test]
    fn closing_lower_untracked_live_pane_redirties_resync_after_higher_reclaim() {
        let mut ledger = ledger(1);
        ledger.mark_dirty(100);
        let close_ack = close_ack(ledger.close_pane(100));
        assert_eq!(
            ledger.acknowledge_pane_close(100, close_ack),
            ReclaimPaneOutcome::Reclaimed
        );
        assert_eq!(ledger.request_resync_all(), ResyncOutcome::Requested);
        let resync = claim(&mut ledger);

        assert_eq!(ledger.close_pane(1), ClosePaneOutcome::Untracked);
        assert!(matches!(
            ledger.resync_all_state(),
            DeliveryState::InFlight {
                redirtied: true,
                ..
            }
        ));
        assert_eq!(ledger.commit(resync), SettleOutcome::RequeuedDirty);
        assert!(!ledger.is_closed());
        assert_eq!(ledger.check_invariants(), Ok(()));
    }

    #[test]
    fn close_ack_rejects_early_wrong_generation_wrong_pane_stale_and_duplicate() {
        let mut ledger = ledger(2);
        ledger.mark_dirty(10);
        ledger.mark_dirty(11);

        let early = PaneCloseAckToken {
            instance: ledger.instance(),
            generation: generation(),
            pane_id: 10,
            sequence: 1,
        };
        assert_eq!(
            ledger.acknowledge_pane_close(10, early),
            ReclaimPaneOutcome::AwaitingClose,
            "even a guessed future token cannot acknowledge a pane before it closes"
        );
        let exact = close_ack(ledger.close_pane(10));
        assert_eq!(exact, early);

        let wrong_generation = PaneCloseAckToken {
            generation: DeliveryGeneration::new(generation().get() + 1),
            ..exact
        };
        assert_eq!(
            ledger.acknowledge_pane_close(10, wrong_generation),
            ReclaimPaneOutcome::WrongGeneration
        );
        assert_eq!(
            ledger.acknowledge_pane_close(11, exact),
            ReclaimPaneOutcome::WrongPane
        );
        let stale = PaneCloseAckToken {
            sequence: exact.sequence() - 1,
            ..exact
        };
        assert_eq!(
            ledger.acknowledge_pane_close(10, stale),
            ReclaimPaneOutcome::StaleOrDuplicate
        );
        assert_eq!(
            ledger.acknowledge_pane_close(10, exact),
            ReclaimPaneOutcome::Reclaimed
        );
        assert_eq!(
            ledger.acknowledge_pane_close(10, exact),
            ReclaimPaneOutcome::StaleOrDuplicate
        );

        assert_eq!(ledger.counters().close_acknowledgements, 1);
        assert_eq!(ledger.counters().rejected_close_acknowledgements, 5);
        assert_eq!(ledger.check_invariants(), Ok(()));
    }

    #[test]
    fn same_generation_cross_instance_claim_cannot_commit() {
        let mut original = ledger(1);
        original.mark_dirty(7);
        let stale = claim(&mut original);

        let mut replacement = ledger(1);
        replacement.mark_dirty(7);
        let current = claim(&mut replacement);
        assert_ne!(stale.token().instance(), current.token().instance());

        assert_eq!(replacement.commit(stale), SettleOutcome::StaleOrDuplicate);
        assert!(matches!(
            replacement.pane_state(7),
            Some(DeliveryState::InFlight { token, .. }) if token == current.token()
        ));
        assert_eq!(
            replacement.counters().rejected_cross_instance_settlements,
            1
        );
        assert_eq!(replacement.commit(current), SettleOutcome::CommittedClean);
    }

    #[test]
    fn same_generation_cross_instance_claim_cannot_retry_or_settle_no_change() {
        let mut original = ledger(1);
        original.mark_dirty(8);
        let stale = claim(&mut original);

        let mut replacement = ledger(1);
        replacement.mark_dirty(8);
        let current = claim(&mut replacement);

        assert_eq!(replacement.retry(stale), SettleOutcome::StaleOrDuplicate);
        assert_eq!(
            replacement.settle_no_change(stale),
            SettleOutcome::StaleOrDuplicate
        );
        assert!(matches!(
            replacement.pane_state(8),
            Some(DeliveryState::InFlight { token, .. }) if token == current.token()
        ));
        assert_eq!(
            replacement.counters().rejected_cross_instance_settlements,
            2
        );
        assert_eq!(
            replacement.settle_no_change(current),
            SettleOutcome::LocallySettledNoChange
        );
    }

    #[test]
    fn same_generation_cross_instance_terminal_failure_cannot_close_replacement() {
        let mut original = ledger(1);
        original.mark_dirty(9);
        let stale = claim(&mut original);

        let mut replacement = ledger(1);
        replacement.mark_dirty(9);
        let current = claim(&mut replacement);

        assert_eq!(
            replacement.fail_terminal(stale),
            SettleOutcome::StaleOrDuplicate
        );
        assert!(!replacement.is_closed());
        assert!(matches!(
            replacement.pane_state(9),
            Some(DeliveryState::InFlight { token, .. }) if token == current.token()
        ));
        assert_eq!(
            replacement.counters().rejected_cross_instance_settlements,
            1
        );
    }

    #[test]
    fn same_generation_cross_instance_close_ack_cannot_reclaim_replacement() {
        let mut original = ledger(1);
        original.mark_dirty(10);
        let stale = close_ack(original.close_pane(10));

        let mut replacement = ledger(1);
        replacement.mark_dirty(10);
        let current = close_ack(replacement.close_pane(10));
        assert_ne!(stale.instance(), current.instance());

        assert_eq!(
            replacement.acknowledge_pane_close(10, stale),
            ReclaimPaneOutcome::WrongInstance
        );
        assert_eq!(replacement.pane_state(10), Some(DeliveryState::Closed));
        assert_eq!(
            replacement
                .counters()
                .rejected_cross_instance_close_acknowledgements,
            1
        );
        assert_eq!(replacement.counters().rejected_close_acknowledgements, 1);
        assert_eq!(
            replacement.acknowledge_pane_close(10, current),
            ReclaimPaneOutcome::Reclaimed
        );
    }

    #[test]
    fn closed_replacement_still_classifies_cross_instance_settlements() {
        let mut original = ledger(1);
        original.mark_dirty(11);
        let stale = claim(&mut original);

        let mut replacement = ledger(1);
        replacement.close_generation();

        assert_eq!(replacement.commit(stale), SettleOutcome::StaleOrDuplicate);
        assert_eq!(
            replacement.settle_no_change(stale),
            SettleOutcome::StaleOrDuplicate
        );
        assert_eq!(replacement.retry(stale), SettleOutcome::StaleOrDuplicate);
        assert_eq!(
            replacement.fail_terminal(stale),
            SettleOutcome::StaleOrDuplicate
        );
        assert_eq!(
            replacement.counters().rejected_cross_instance_settlements,
            4
        );
        assert!(replacement.is_closed());
    }

    #[test]
    fn closed_replacement_still_classifies_cross_instance_close_ack() {
        let mut original = ledger(1);
        original.mark_dirty(12);
        let stale = close_ack(original.close_pane(12));

        let mut replacement = ledger(1);
        replacement.close_generation();

        assert_eq!(
            replacement.acknowledge_pane_close(12, stale),
            ReclaimPaneOutcome::WrongInstance
        );
        assert_eq!(
            replacement
                .counters()
                .rejected_cross_instance_close_acknowledgements,
            1
        );
        assert_eq!(replacement.counters().rejected_close_acknowledgements, 1);
        assert!(replacement.is_closed());
    }

    #[test]
    fn stale_and_duplicate_commits_cannot_clean_newer_work() {
        let mut ledger = ledger(1);
        ledger.mark_dirty(7);
        let first = claim(&mut ledger);
        assert_eq!(ledger.commit(first), SettleOutcome::CommittedClean);
        assert_eq!(ledger.commit(first), SettleOutcome::StaleOrDuplicate);

        ledger.mark_dirty(7);
        let second = claim(&mut ledger);
        assert_eq!(ledger.commit(first), SettleOutcome::StaleOrDuplicate);
        assert!(matches!(
            ledger.pane_state(7),
            Some(DeliveryState::InFlight { token, .. }) if token == second.token()
        ));
        assert_eq!(ledger.retry(second), SettleOutcome::RequeuedDirty);
    }

    #[test]
    fn no_change_settlement_is_local_pane_only_and_preserves_redirty() {
        let mut clean = ledger(1);
        clean.mark_dirty(7);
        let no_change_claim = claim(&mut clean);
        assert_eq!(
            clean.settle_no_change(no_change_claim),
            SettleOutcome::LocallySettledNoChange
        );
        assert_eq!(clean.pane_state(7), Some(DeliveryState::Clean));
        assert_eq!(clean.counters().commits, 0);
        assert_eq!(clean.counters().no_change_settlements, 1);

        let mut redirtied = ledger(1);
        redirtied.mark_dirty(8);
        let redirtied_claim = claim(&mut redirtied);
        redirtied.mark_dirty(8);
        assert_eq!(
            redirtied.settle_no_change(redirtied_claim),
            SettleOutcome::RequeuedDirty
        );
        assert_eq!(claim(&mut redirtied).scope(), DeliveryScope::Pane(8));

        let mut resync = ledger(0);
        resync.request_resync_all();
        let resync_claim = claim(&mut resync);
        assert_eq!(
            resync.settle_no_change(resync_claim),
            SettleOutcome::StaleOrDuplicate,
            "authoritative resync cannot be silently settled without application evidence"
        );
        assert!(matches!(
            resync.resync_all_state(),
            DeliveryState::InFlight { .. }
        ));
    }

    #[test]
    fn shutdown_closes_dirty_and_inflight_work_without_false_clean() {
        let mut ledger = ledger(2);
        ledger.mark_dirty(1);
        ledger.mark_dirty(2);
        let old = claim(&mut ledger);
        ledger.close_generation();

        assert!(ledger.is_closed());
        assert_eq!(ledger.pane_state(1), Some(DeliveryState::Closed));
        assert_eq!(ledger.pane_state(2), Some(DeliveryState::Closed));
        assert_eq!(ledger.mark_dirty(1), DirtyOutcome::IgnoredClosed);
        assert_eq!(ledger.claim_next(), Ok(None));
        assert_eq!(ledger.commit(old), SettleOutcome::GenerationClosed);
        assert_eq!(ledger.check_invariants(), Ok(()));
    }

    #[test]
    fn shutdown_closes_dirty_and_inflight_resync_without_settlement() {
        let mut dirty_resync = ledger(0);
        dirty_resync.mark_dirty(1);
        assert_eq!(dirty_resync.resync_all_state(), DeliveryState::Dirty);
        dirty_resync.close_generation();
        assert_eq!(dirty_resync.resync_all_state(), DeliveryState::Closed);
        assert_eq!(dirty_resync.claim_next(), Ok(None));

        let mut inflight_resync = ledger(0);
        inflight_resync.mark_dirty(1);
        let old = claim(&mut inflight_resync);
        assert_eq!(old.scope(), DeliveryScope::ResyncAll);
        inflight_resync.close_generation();
        assert_eq!(inflight_resync.commit(old), SettleOutcome::GenerationClosed);
        assert_eq!(inflight_resync.retry(old), SettleOutcome::GenerationClosed);
        assert_eq!(inflight_resync.check_invariants(), Ok(()));
    }

    #[test]
    fn dirty_for_closed_pane_does_not_redirty_inflight_resync() {
        let mut ledger = ledger(1);
        ledger.mark_dirty(1);
        assert!(matches!(
            ledger.close_pane(1),
            ClosePaneOutcome::ClosedDirty { .. }
        ));
        assert_eq!(ledger.request_resync_all(), ResyncOutcome::Requested);
        let resync = claim(&mut ledger);
        assert_eq!(resync.scope(), DeliveryScope::ResyncAll);

        assert_eq!(ledger.mark_dirty(1), DirtyOutcome::IgnoredClosed);
        assert!(matches!(
            ledger.resync_all_state(),
            DeliveryState::InFlight {
                redirtied: false,
                ..
            }
        ));
        assert_eq!(ledger.commit(resync), SettleOutcome::CommittedClean);
    }

    #[test]
    fn untracked_close_redirties_an_inflight_all_pane_inventory() {
        let mut ledger = ledger(0);
        assert_eq!(ledger.request_resync_all(), ResyncOutcome::Requested);
        let resync = claim(&mut ledger);
        assert_eq!(resync.scope(), DeliveryScope::ResyncAll);

        assert_eq!(ledger.close_pane(41), ClosePaneOutcome::Untracked);
        assert!(matches!(
            ledger.resync_all_state(),
            DeliveryState::InFlight {
                redirtied: true,
                ..
            }
        ));
        assert_eq!(ledger.commit(resync), SettleOutcome::RequeuedDirty);
        assert_eq!(claim(&mut ledger).scope(), DeliveryScope::ResyncAll);
        assert_eq!(ledger.check_invariants(), Ok(()));
    }

    #[test]
    fn token_exhaustion_fails_closed_without_wrap_or_reuse() {
        let mut ledger = ledger(1);
        ledger.set_next_token_sequence_for_test(Some(u64::MAX));
        ledger.mark_dirty(1);
        let final_token = claim(&mut ledger);
        assert_eq!(final_token.token().sequence(), u64::MAX);
        assert_eq!(ledger.commit(final_token), SettleOutcome::CommittedClean);

        ledger.mark_dirty(1);
        assert_eq!(ledger.claim_next(), Err(ClaimError::TokenExhausted));
        assert!(ledger.is_closed());
        assert_eq!(ledger.counters().terminal_failures, 1);
        assert_eq!(ledger.check_invariants(), Ok(()));
    }

    #[test]
    fn close_ack_token_exhaustion_fails_generation_closed_without_reuse() {
        let mut ledger = ledger(2);
        ledger.set_next_close_ack_sequence_for_test(Some(u64::MAX));
        ledger.mark_dirty(1);
        ledger.mark_dirty(2);

        let final_ack = close_ack(ledger.close_pane(1));
        assert_eq!(final_ack.sequence(), u64::MAX);
        assert_eq!(
            ledger.close_pane(2),
            ClosePaneOutcome::GenerationClosed,
            "exhausted close-ACK space must close rather than reuse a capability"
        );
        assert!(ledger.is_closed());
        assert_eq!(ledger.counters().terminal_failures, 1);
        assert_eq!(ledger.check_invariants(), Ok(()));
    }

    #[test]
    fn terminal_failure_fails_closed_and_transient_failure_retries() {
        let mut transient = ledger(1);
        transient.mark_dirty(1);
        let transient_claim = claim(&mut transient);
        assert_eq!(
            transient.retry(transient_claim),
            SettleOutcome::RequeuedDirty
        );
        assert!(!transient.is_closed());
        assert_eq!(transient.counters().retries, 1);

        let terminal_claim = claim(&mut transient);
        assert_eq!(
            transient.fail_terminal(terminal_claim),
            SettleOutcome::FailedClosed
        );
        assert!(transient.is_closed());
        assert_eq!(transient.counters().terminal_failures, 1);
    }

    #[test]
    fn same_generation_transport_death_closes_even_after_scope_race() {
        let mut ledger = ledger(1);
        ledger.mark_dirty(1);
        let transport_claim = claim(&mut ledger);
        assert!(matches!(
            ledger.close_pane(1),
            ClosePaneOutcome::ClosedInFlight { .. }
        ));
        assert_eq!(
            ledger.fail_terminal(transport_claim),
            SettleOutcome::FailedClosed
        );
        assert!(ledger.is_closed());

        let mut new_generation = ledger_for(DeliveryGeneration::new(18), 1);
        new_generation.mark_dirty(1);
        assert_eq!(
            new_generation.fail_terminal(transport_claim),
            SettleOutcome::StaleOrDuplicate
        );
        assert!(!new_generation.is_closed());
    }

    #[test]
    fn notification_effects_make_saturation_semantics_explicit() {
        let render_notification = MuxNotification::PaneOutput(1);
        let render = notification_effects(&render_notification);
        assert_eq!(
            render.primary.on_full,
            FullQueueContract::PreserveDurableObligation
        );
        assert_eq!(
            render.primary.coalescing,
            NotificationCoalescing::PerPaneDirtyBit
        );
        assert_eq!(
            render.primary.fairness,
            NotificationFairness::PaneRoundRobin
        );
        assert_eq!(render.render_key, Some(NotificationRenderKey::Pane(1)));
        assert_eq!(
            render.primary.on_closed,
            ClosedQueueContract::RejectAndCloseGeneration
        );

        let lifecycle_notification = MuxNotification::PaneRemoved(1);
        let lifecycle = notification_effects(&lifecycle_notification);
        assert_eq!(
            lifecycle.primary.on_full,
            FullQueueContract::JournalThenResyncAuthoritativeState
        );
        assert_eq!(
            lifecycle.primary.coalescing,
            NotificationCoalescing::Forbidden
        );
        assert_eq!(
            lifecycle.primary.fairness,
            NotificationFairness::BoundedPriorityFifo
        );
        assert_eq!(
            lifecycle.topology,
            NotificationTopologyEffect::PaneRemovedTombstone(1)
        );
        assert_eq!(
            lifecycle.admission,
            NotificationAdmissionContract::PostMutationJournalThenTopologyResync
        );

        let bell_notification = MuxNotification::Alert {
            pane_id: 1,
            alert: Alert::Bell,
        };
        let bell = notification_effects(&bell_notification);
        assert_eq!(bell.primary.class, NotificationClass::PayloadEvent);
        assert_eq!(
            bell.event,
            NotificationEventEffect::StrictFifo(NotificationEventKey::Bell(1))
        );
        assert_eq!(
            bell.render_key,
            Some(NotificationRenderKey::Pane(1)),
            "bell is both an exact occurrence and a transient render effect"
        );
        assert_ne!(
            bell.primary.on_full,
            FullQueueContract::PreserveDurableObligation,
            "noncoalescible events must not silently become a render resync"
        );

        let empty_notification = MuxNotification::Empty;
        let empty = notification_effects(&empty_notification);
        assert_eq!(
            empty.primary.coalescing,
            NotificationCoalescing::AuthoritativeLevelPredicate
        );
        assert_eq!(
            empty.level_predicate,
            NotificationLevelPredicate::MuxBecameEmpty
        );
        assert_eq!(
            empty.primary.on_full,
            FullQueueContract::PreserveDurableObligation
        );
        assert_eq!(
            empty.primary.on_closed,
            ClosedQueueContract::RejectAndCloseGeneration,
            "semantic Empty must never use the scheduler wake-only close rule"
        );
        assert!(DELIVERY_PRIORITY_BURST_LIMIT > 0);
    }

    #[test]
    fn alert_effects_distinguish_state_render_auxiliary_and_event_lanes() {
        let palette_notification = MuxNotification::Alert {
            pane_id: 7,
            alert: Alert::PaletteChanged,
        };
        let palette = notification_effects(&palette_notification);
        assert_eq!(
            palette.state_keys,
            [Some(NotificationStateKey::PanePalette(7)), None]
        );
        assert_eq!(palette.render_key, Some(NotificationRenderKey::Pane(7)));
        assert_eq!(
            palette.auxiliary_snapshot,
            Some(AuxiliarySnapshotKey::PanePalette(7))
        );
        assert_eq!(palette.event, NotificationEventEffect::None);

        let user_var_notification = MuxNotification::Alert {
            pane_id: 7,
            alert: Alert::SetUserVar {
                name: "build".to_string(),
                value: "green".to_string(),
            },
        };
        let user_var = notification_effects(&user_var_notification);
        assert_eq!(
            user_var.state_keys,
            [
                Some(NotificationStateKey::PaneUserVar {
                    pane_id: 7,
                    name: "build",
                }),
                None,
            ]
        );
        assert_eq!(
            user_var.event,
            NotificationEventEffect::StateAndStrictFifo(NotificationEventKey::PaneUserVar {
                pane_id: 7,
                name: "build",
            })
        );
        assert_eq!(
            user_var.primary.coalescing,
            NotificationCoalescing::StateAndStrictFifoEvent
        );

        let progress_42_notification = MuxNotification::Alert {
            pane_id: 7,
            alert: Alert::Progress(wezterm_term::Progress::Percentage(42)),
        };
        let progress_64_notification = MuxNotification::Alert {
            pane_id: 7,
            alert: Alert::Progress(wezterm_term::Progress::Percentage(64)),
        };
        let progress_42 = notification_effects(&progress_42_notification);
        let progress_64 = notification_effects(&progress_64_notification);
        assert_eq!(progress_42.state_keys, progress_64.state_keys);
        assert_eq!(
            progress_64.state_keys,
            [Some(NotificationStateKey::PaneProgress(7)), None]
        );
        assert_eq!(
            progress_64.primary.coalescing,
            NotificationCoalescing::LatestPerStableKey,
            "newer progress values replace older state; a timer must not drop them"
        );
    }

    #[test]
    fn telemetry_and_external_commands_have_non_item_count_recovery() {
        let synchronized_notification = MuxNotification::SynchronizedOutput {
            pane_id: 9,
            event: mux::SynchronizedOutputEvent::ModeQuery,
        };
        let synchronized = notification_effects(&synchronized_notification);
        assert_eq!(
            synchronized.event,
            NotificationEventEffect::LosslessSynchronizedOutputFold { pane_id: 9 }
        );
        assert_eq!(
            synchronized.primary.ordering,
            NotificationOrdering::CausallyBeforePaneRender
        );
        assert_eq!(
            synchronized.primary.on_full,
            FullQueueContract::FoldIntoDurableAccumulator
        );

        let download_notification = MuxNotification::SaveToDownloads {
            name: Some("trace.bin".to_string()),
            data: Arc::new(vec![0; 4097]),
        };
        let download = notification_effects(&download_notification);
        assert_eq!(
            download.command,
            NotificationCommandEffect::SaveToDownloads {
                payload_bytes: 4097
            }
        );
        assert_eq!(
            download.primary.on_full,
            FullQueueContract::RequireByteBudgetPermitOrSpool
        );
        assert_eq!(
            download.admission,
            NotificationAdmissionContract::ByteBudgetPermitOrDurableSpool
        );
    }

    proptest! {
        #[test]
        fn reclaiming_one_id_never_changes_a_distinct_live_id(
            reclaimed_id in 0usize..=4096,
            live_id in 0usize..=4096,
        ) {
            prop_assume!(reclaimed_id != live_id);
            let mut ledger = ledger(1);
            prop_assert_eq!(
                ledger.mark_dirty(reclaimed_id),
                DirtyOutcome::BecameDirty
            );
            let close_ack = close_ack(ledger.close_pane(reclaimed_id));
            prop_assert_eq!(
                ledger.acknowledge_pane_close(reclaimed_id, close_ack),
                ReclaimPaneOutcome::Reclaimed
            );

            prop_assert_eq!(
                ledger.mark_dirty(live_id),
                DirtyOutcome::BecameDirty,
                "reclaiming {} changed distinct live pane {}",
                reclaimed_id,
                live_id
            );
            prop_assert!(!ledger.is_closed());
            let instance = ledger.instance();
            prop_assert_eq!(
                ledger.claim_next(),
                Ok(Some(DeliveryClaim {
                    scope: DeliveryScope::Pane(live_id),
                    token: DeliveryToken {
                        instance,
                        generation: generation(),
                        sequence: 1,
                    },
                }))
            );
            prop_assert_eq!(ledger.check_invariants(), Ok(()));
        }

        #[test]
        fn generated_transition_sequences_preserve_all_structural_invariants(
            pane_limit in 0usize..=8,
            actions in prop::collection::vec((0u8..=6, 0usize..=12), 0..256),
        ) {
            let mut ledger = ledger(pane_limit);
            let mut claims = Vec::new();

            for (action, pane_id) in actions {
                match action {
                    0 => {
                        let _ = ledger.mark_dirty(pane_id);
                    }
                    1 => {
                        if let Ok(Some(next)) = ledger.claim_next() {
                            claims.push(next);
                        }
                    }
                    2 => {
                        if let Some(current) = claims.pop() {
                            let _ = ledger.commit(current);
                        }
                    }
                    3 => {
                        if let Some(current) = claims.pop() {
                            let _ = ledger.retry(current);
                        }
                    }
                    4 => {
                        let _ = ledger.close_pane(pane_id);
                    }
                    5 => {
                        let _ = ledger.request_resync_all();
                    }
                    6 => ledger.close_generation(),
                    _ => unreachable!("generated action is constrained to 0..=6"),
                }

                prop_assert!(
                    ledger.capacity().tracked_panes <= pane_limit,
                    "tracked pane bound must hold after every transition"
                );
                prop_assert!(
                    ledger.capacity().ready_panes <= pane_limit,
                    "ready queue bound must hold after every transition"
                );
                prop_assert_eq!(ledger.check_invariants(), Ok(()));
            }
        }
    }
}
