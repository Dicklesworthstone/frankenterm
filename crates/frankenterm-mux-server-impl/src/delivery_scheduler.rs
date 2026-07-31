//! Executable bounded scheduler models for mux delivery classes.
//!
//! This module is the scheduling half of `ft.render-delivery-ledger.v1`.  It
//! deliberately has no live mux wiring: it freezes the admission, capacity,
//! ordering, saturation, wake, fairness, and shutdown behavior that a later
//! integration must preserve.
//!
//! Durable work is admitted before a producer may report success:
//!
//! - lifecycle barriers and payload events use independent bounded strict-FIFO
//!   lanes; a full lane rejects the producer without mutating the lane;
//! - state and render updates retain the latest value per stable key and visit
//!   keys round-robin;
//! - overflow of either keyed lane sets that lane's single fixed-size
//!   authoritative-resync slot.  The resync supersedes the retained keyed
//!   values when scheduled;
//! - a wake is a coalesced one-bit hint.  It never owns semantic payload and
//!   may therefore be consumed or coalesced independently of durable work.
//!
//! Cross-class service follows priority order in bounded bursts.  A
//! continuously ready durable class is selected after at most
//! [`DELIVERY_MAX_STARVATION_DEQUEUES`] selections from other classes.  FIFO
//! and stable-key ordering within each class are unaffected by this bound.
//!
//! The original [`DeliveryScheduler`] remains the small v1 class-arbitration
//! model.  It is intentionally preserved because its exact queue and fairness
//! semantics are useful as a comparison baseline.  It is **not** the authority
//! for lossless delivery: destructive dequeue cannot represent application
//! acknowledgement.
//!
//! [`DeliveryCoordinator`] is the hardened v2 contract.  It stores only
//! fixed-size authority handles, charges both slots and resident bytes, admits
//! multi-effect plans atomically, and retains the authoritative plan until a
//! generation-and-attempt-bound claim is acknowledged.  A single causal FIFO
//! with explicit epochs is conservative by design: lifecycle barriers and
//! telemetry fences may cost concurrency, but can never be overtaken.

use crate::delivery_ledger::{
    DELIVERY_PRIORITY_BURST_LIMIT, DeliveryClaim, DeliveryLedgerInstance, DeliveryScope,
    PendingDeliveryWake,
};
use codec::{
    RenderApplicationContractError, RenderApplicationIdentity, RenderApplicationKind,
    RenderApplicationNackReason, RenderApplicationNackRecovery, RenderApplicationOutcome,
    RenderApplicationResult, RenderApplicationUpdate, RenderConnectionIdentity,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::Hash;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::task::{Context, Poll, Waker};

/// Stable identifier for this executable scheduler contract.
pub const DELIVERY_SCHEDULER_CONTRACT_VERSION: &str = "ft.delivery-scheduler.v1";

/// Number of durable scheduling classes.
pub const DELIVERY_DURABLE_CLASS_COUNT: usize = 4;

/// Maximum number of other dequeues that may precede a continuously ready
/// durable class.
///
/// The scheduler visits four priority bands cyclically and permits at most
/// [`DELIVERY_PRIORITY_BURST_LIMIT`] consecutive selections per ready band.
pub const DELIVERY_MAX_STARVATION_DEQUEUES: usize =
    (DELIVERY_DURABLE_CLASS_COUNT - 1) * DELIVERY_PRIORITY_BURST_LIMIT;

const DURABLE_CLASSES: [DurableClass; DELIVERY_DURABLE_CLASS_COUNT] = [
    DurableClass::Lifecycle,
    DurableClass::State,
    DurableClass::Render,
    DurableClass::Payload,
];

/// Durable notification classes in descending scheduling priority.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DurableClass {
    Lifecycle,
    State,
    Render,
    Payload,
}

impl DurableClass {
    const fn index(self) -> usize {
        match self {
            Self::Lifecycle => 0,
            Self::State => 1,
            Self::Render => 2,
            Self::Payload => 3,
        }
    }
}

/// Logical capacities for the four bounded data lanes.
///
/// Each keyed lane also owns exactly one authoritative-resync slot, even when
/// its key limit is zero.  The wake hint owns one additional non-durable slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SchedulerLimits {
    pub lifecycle: usize,
    pub state_keys: usize,
    pub render_keys: usize,
    pub payload: usize,
}

impl SchedulerLimits {
    #[must_use]
    pub const fn new(
        lifecycle: usize,
        state_keys: usize,
        render_keys: usize,
        payload: usize,
    ) -> Self {
        Self {
            lifecycle,
            state_keys,
            render_keys,
            payload,
        }
    }

    fn durable_limit(self) -> Option<usize> {
        self.lifecycle
            .checked_add(self.state_keys)?
            .checked_add(1)?
            .checked_add(self.render_keys)?
            .checked_add(1)?
            .checked_add(self.payload)
    }
}

/// Construction fails rather than reporting a wrapped aggregate capacity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SchedulerConfigError {
    CapacityOverflow,
}

/// Result of attempting to publish durable work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdmissionOutcome {
    /// A new durable lane entry was admitted.
    Admitted,
    /// Existing durable state already covered the publication.
    Coalesced,
    /// A keyed-lane overflow installed its authoritative-resync obligation.
    Escalated,
    /// A noncoalescible FIFO lane was full; the producer must apply
    /// backpressure or fail explicitly.
    Rejected,
    /// The scheduler is terminally closed; a new generation is required.
    Closed,
}

/// Admission result that returns ownership when a producer was rejected.
///
/// This mirrors bounded-channel `try_send` semantics: observing
/// [`AdmissionOutcome::Rejected`] or [`AdmissionOutcome::Closed`] never
/// destroys the producer's non-admitted value.
#[derive(Clone, Debug, Eq, PartialEq)]
#[must_use = "admission must be inspected so rejected or closed work is not silently lost"]
pub struct Admission<T> {
    outcome: AdmissionOutcome,
    returned: Option<T>,
}

impl<T> Admission<T> {
    fn accepted(outcome: AdmissionOutcome) -> Self {
        debug_assert!(matches!(
            outcome,
            AdmissionOutcome::Admitted | AdmissionOutcome::Coalesced | AdmissionOutcome::Escalated
        ));
        Self {
            outcome,
            returned: None,
        }
    }

    fn returned(outcome: AdmissionOutcome, value: T) -> Self {
        debug_assert!(matches!(
            outcome,
            AdmissionOutcome::Rejected | AdmissionOutcome::Closed
        ));
        Self {
            outcome,
            returned: Some(value),
        }
    }

    #[must_use]
    pub const fn outcome(&self) -> AdmissionOutcome {
        self.outcome
    }

    /// Recover a value that was not admitted.
    #[must_use]
    pub fn into_returned(self) -> Option<T> {
        self.returned
    }

    /// Borrow a value that was not admitted.
    #[must_use]
    pub fn returned_value(&self) -> Option<&T> {
        self.returned.as_ref()
    }
}

impl<T> PartialEq<AdmissionOutcome> for Admission<T> {
    fn eq(&self, other: &AdmissionOutcome) -> bool {
        self.outcome == *other
    }
}

/// Result of publishing the payload-free wake hint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WakeOutcome {
    Admitted,
    Coalesced,
    Closed,
}

/// Result of terminally closing the local scheduler generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SchedulerCloseOutcome {
    Closed { discarded_obligations: usize },
    AlreadyClosed,
}

/// One scheduled durable item.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScheduledItem<L, StateKey, StateValue, RenderKey, RenderValue, P> {
    Lifecycle(L),
    State { key: StateKey, value: StateValue },
    StateResync,
    Render { key: RenderKey, value: RenderValue },
    RenderResync,
    Payload(P),
}

impl<L, StateKey, StateValue, RenderKey, RenderValue, P>
    ScheduledItem<L, StateKey, StateValue, RenderKey, RenderValue, P>
{
    #[must_use]
    pub const fn class(&self) -> DurableClass {
        match self {
            Self::Lifecycle(_) => DurableClass::Lifecycle,
            Self::State { .. } | Self::StateResync => DurableClass::State,
            Self::Render { .. } | Self::RenderResync => DurableClass::Render,
            Self::Payload(_) => DurableClass::Payload,
        }
    }
}

/// Exact logical usage for one bounded lane or fixed slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LaneCapacity {
    pub limit: usize,
    pub used: usize,
}

/// Exact logical capacity accounting for every lane and fixed slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SchedulerCapacity {
    pub lifecycle: LaneCapacity,
    pub state_keys: LaneCapacity,
    pub state_resync: LaneCapacity,
    pub render_keys: LaneCapacity,
    pub render_resync: LaneCapacity,
    pub payload: LaneCapacity,
    pub durable_total: LaneCapacity,
    pub wake: LaneCapacity,
}

/// Monotonic counters for one durable class.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ClassCounters {
    pub admitted: u64,
    pub coalesced: u64,
    pub escalated: u64,
    pub rejected: u64,
    pub closed: u64,
    pub dequeued: u64,
}

impl ClassCounters {
    fn record_admission(&mut self, outcome: AdmissionOutcome) {
        match outcome {
            AdmissionOutcome::Admitted => increment(&mut self.admitted),
            AdmissionOutcome::Coalesced => increment(&mut self.coalesced),
            AdmissionOutcome::Escalated => increment(&mut self.escalated),
            AdmissionOutcome::Rejected => increment(&mut self.rejected),
            AdmissionOutcome::Closed => increment(&mut self.closed),
        }
    }
}

/// Monotonic counters for the payload-free wake slot.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WakeCounters {
    pub admitted: u64,
    pub coalesced: u64,
    pub closed: u64,
    pub consumed: u64,
}

/// Monotonic observability counters required by the scheduler contract.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SchedulerCounters {
    pub lifecycle: ClassCounters,
    pub state: ClassCounters,
    pub render: ClassCounters,
    pub payload: ClassCounters,
    pub wake: WakeCounters,
    pub shutdowns: u64,
    pub discarded_on_shutdown: u64,
}

fn increment(counter: &mut u64) {
    *counter = counter.saturating_add(1);
}

fn add_usize(counter: &mut u64, amount: usize) {
    *counter = counter.saturating_add(u64::try_from(amount).unwrap_or(u64::MAX));
}

#[derive(Clone, Debug)]
struct KeyedSlot<K, V> {
    key: K,
    value: V,
    epoch: u64,
    previous: Option<usize>,
    next: Option<usize>,
}

#[derive(Clone, Debug)]
struct KeyedLane<K, V> {
    limit: usize,
    by_key: HashMap<K, usize>,
    slots: Vec<Option<KeyedSlot<K, V>>>,
    free: Vec<usize>,
    active_head: Option<usize>,
    active_tail: Option<usize>,
    active_len: usize,
    retired_head: Option<usize>,
    retired_tail: Option<usize>,
    retired_len: usize,
    active_epoch: u64,
    resync_pending: bool,
}

impl<K, V> KeyedLane<K, V>
where
    K: Clone + Eq + Hash,
{
    fn new(limit: usize) -> Self {
        Self {
            limit,
            by_key: HashMap::new(),
            slots: Vec::new(),
            free: Vec::new(),
            active_head: None,
            active_tail: None,
            active_len: 0,
            retired_head: None,
            retired_tail: None,
            retired_len: 0,
            active_epoch: 1,
            resync_pending: false,
        }
    }

    fn admit(&mut self, key: K, value: V) -> AdmissionOutcome {
        if self.resync_pending {
            return AdmissionOutcome::Coalesced;
        }

        if let Some(index) = self.by_key.get(&key).copied() {
            let is_active = self.slots[index]
                .as_ref()
                .is_some_and(|slot| slot.epoch == self.active_epoch);
            if is_active {
                self.slots[index]
                    .as_mut()
                    .expect("key index must reference an occupied slot")
                    .value = value;
                return AdmissionOutcome::Coalesced;
            }

            self.unlink_retired(index);
            {
                let slot = self.slots[index]
                    .as_mut()
                    .expect("retired key index must reference an occupied slot");
                slot.value = value;
                slot.epoch = self.active_epoch;
            }
            self.append_active(index);
            return AdmissionOutcome::Admitted;
        }

        if self.active_len < self.limit {
            let index = if let Some(index) = self.free.pop() {
                index
            } else if let Some(index) = self.retired_head {
                self.unlink_retired(index);
                let retired = self.slots[index]
                    .take()
                    .expect("retired list must reference an occupied slot");
                let removed = self.by_key.remove(&retired.key);
                debug_assert_eq!(removed, Some(index));
                index
            } else {
                let index = self.slots.len();
                self.slots.push(None);
                index
            };
            self.slots[index] = Some(KeyedSlot {
                key: key.clone(),
                value,
                epoch: self.active_epoch,
                previous: None,
                next: None,
            });
            let replaced = self.by_key.insert(key, index);
            debug_assert!(replaced.is_none());
            self.append_active(index);
            AdmissionOutcome::Admitted
        } else {
            self.resync_pending = true;
            AdmissionOutcome::Escalated
        }
    }

    fn is_ready(&self) -> bool {
        self.resync_pending || self.active_len != 0
    }

    fn pop(&mut self) -> Option<KeyedPop<K, V>> {
        if self.resync_pending {
            self.resync_pending = false;
            self.retire_active();
            return Some(KeyedPop::Resync);
        }

        let index = self.active_head?;
        self.unlink_active(index);
        let slot = self.slots[index]
            .take()
            .expect("active list must reference an occupied slot");
        let removed = self.by_key.remove(&slot.key);
        debug_assert_eq!(removed, Some(index));
        self.free.push(index);
        Some(KeyedPop::Value {
            key: slot.key,
            value: slot.value,
        })
    }

    fn clear(&mut self) {
        self.by_key.clear();
        self.slots.clear();
        self.free.clear();
        self.active_head = None;
        self.active_tail = None;
        self.active_len = 0;
        self.retired_head = None;
        self.retired_tail = None;
        self.retired_len = 0;
        self.resync_pending = false;
    }

    fn len(&self) -> usize {
        self.active_len
    }

    fn append_active(&mut self, index: usize) {
        let previous_tail = self.active_tail;
        {
            let slot = self.slots[index]
                .as_mut()
                .expect("only occupied slots can enter the active list");
            debug_assert_eq!(slot.epoch, self.active_epoch);
            slot.previous = previous_tail;
            slot.next = None;
        }
        if let Some(tail) = previous_tail {
            self.slots[tail]
                .as_mut()
                .expect("active tail must remain occupied")
                .next = Some(index);
        } else {
            debug_assert!(self.active_head.is_none());
            self.active_head = Some(index);
        }
        self.active_tail = Some(index);
        self.active_len += 1;
    }

    fn unlink_active(&mut self, index: usize) {
        let slot = self.slots[index]
            .as_ref()
            .expect("active index must reference an occupied slot");
        debug_assert_eq!(slot.epoch, self.active_epoch);
        let previous = slot.previous;
        let next = slot.next;
        self.relink_neighbors(previous, next, true);
        let slot = self.slots[index]
            .as_mut()
            .expect("unlinked active slot must remain occupied");
        slot.previous = None;
        slot.next = None;
        self.active_len = self
            .active_len
            .checked_sub(1)
            .expect("active membership charges one slot");
    }

    fn unlink_retired(&mut self, index: usize) {
        let slot = self.slots[index]
            .as_ref()
            .expect("retired index must reference an occupied slot");
        debug_assert_ne!(slot.epoch, self.active_epoch);
        let previous = slot.previous;
        let next = slot.next;
        self.relink_neighbors(previous, next, false);
        let slot = self.slots[index]
            .as_mut()
            .expect("unlinked retired slot must remain occupied");
        slot.previous = None;
        slot.next = None;
        self.retired_len = self
            .retired_len
            .checked_sub(1)
            .expect("retired membership charges one slot");
    }

    fn relink_neighbors(
        &mut self,
        previous: Option<usize>,
        next: Option<usize>,
        active: bool,
    ) {
        if let Some(previous) = previous {
            self.slots[previous]
                .as_mut()
                .expect("list predecessor must remain occupied")
                .next = next;
        } else if active {
            self.active_head = next;
        } else {
            self.retired_head = next;
        }

        if let Some(next) = next {
            self.slots[next]
                .as_mut()
                .expect("list successor must remain occupied")
                .previous = previous;
        } else if active {
            self.active_tail = previous;
        } else {
            self.retired_tail = previous;
        }
    }

    fn retire_active(&mut self) {
        let Some(active_head) = self.active_head else {
            self.advance_epoch();
            return;
        };
        if let Some(retired_tail) = self.retired_tail {
            self.slots[retired_tail]
                .as_mut()
                .expect("retired tail must remain occupied")
                .next = Some(active_head);
            self.slots[active_head]
                .as_mut()
                .expect("active head must remain occupied")
                .previous = Some(retired_tail);
        } else {
            self.retired_head = Some(active_head);
        }
        self.retired_tail = self.active_tail;
        self.retired_len = self
            .retired_len
            .checked_add(self.active_len)
            .expect("key limit bounds retired-slot count");
        self.active_head = None;
        self.active_tail = None;
        self.active_len = 0;
        self.advance_epoch();
    }

    fn advance_epoch(&mut self) {
        if let Some(next) = self.active_epoch.checked_add(1) {
            self.active_epoch = next;
            return;
        }

        // Rebase only after 2^64 resync generations.  This cold exhaustion
        // repair preserves correctness without wraparound aliasing; ordinary
        // resync remains an O(1) list splice.
        for slot in self.slots.iter_mut().flatten() {
            slot.epoch = 0;
        }
        self.active_epoch = 1;
    }

    fn check_invariants(&self) -> Result<(), &'static str> {
        if self.active_len > self.limit
            || self.by_key.len() > self.limit
            || self.slots.len() > self.limit
        {
            return Err("keyed lane exceeds its configured key limit");
        }
        if self.active_len.checked_add(self.retired_len) != Some(self.by_key.len()) {
            return Err("keyed lane list and key-index counts disagree");
        }
        if self.by_key.len().checked_add(self.free.len()) != Some(self.slots.len()) {
            return Err("keyed lane occupied and free slot counts disagree");
        }
        if (self.active_len == 0) != (self.active_head.is_none() && self.active_tail.is_none()) {
            return Err("active keyed-list endpoints disagree with its length");
        }
        if (self.retired_len == 0) != (self.retired_head.is_none() && self.retired_tail.is_none()) {
            return Err("retired keyed-list endpoints disagree with its length");
        }

        let mut seen = HashSet::with_capacity(self.by_key.len());
        let mut cursor = self.active_head;
        let mut previous = None;
        let mut traversed = 0usize;
        while let Some(index) = cursor {
            if traversed == self.active_len || !seen.insert(index) {
                return Err("active keyed list contains a cycle");
            }
            let slot = self
                .slots
                .get(index)
                .and_then(Option::as_ref)
                .ok_or("active keyed list references a vacant slot")?;
            if slot.epoch != self.active_epoch || slot.previous != previous {
                return Err("active keyed-list metadata is inconsistent");
            }
            previous = Some(index);
            cursor = slot.next;
            traversed += 1;
        }
        if traversed != self.active_len || previous != self.active_tail {
            return Err("active keyed-list traversal disagrees with its bounds");
        }

        cursor = self.retired_head;
        previous = None;
        traversed = 0;
        while let Some(index) = cursor {
            if traversed == self.retired_len || !seen.insert(index) {
                return Err("retired keyed list contains a cycle or overlap");
            }
            let slot = self
                .slots
                .get(index)
                .and_then(Option::as_ref)
                .ok_or("retired keyed list references a vacant slot")?;
            if slot.epoch == self.active_epoch || slot.previous != previous {
                return Err("retired keyed-list metadata is inconsistent");
            }
            previous = Some(index);
            cursor = slot.next;
            traversed += 1;
        }
        if traversed != self.retired_len || previous != self.retired_tail {
            return Err("retired keyed-list traversal disagrees with its bounds");
        }

        for (key, index) in &self.by_key {
            let slot = self
                .slots
                .get(*index)
                .and_then(Option::as_ref)
                .ok_or("key index references a vacant slot")?;
            if &slot.key != key || !seen.contains(index) {
                return Err("key index disagrees with stable-slot authority");
            }
        }
        let mut free = HashSet::with_capacity(self.free.len());
        for index in &self.free {
            if *index >= self.slots.len()
                || self.slots[*index].is_some()
                || !free.insert(*index)
            {
                return Err("free list references an occupied, absent, or duplicate slot");
            }
        }
        for (index, slot) in self.slots.iter().enumerate() {
            if slot.is_some() != seen.contains(&index) || slot.is_none() != free.contains(&index) {
                return Err("keyed slot is disconnected from list and free-list authority");
            }
        }
        Ok(())
    }
}

enum KeyedPop<K, V> {
    Value { key: K, value: V },
    Resync,
}

/// Bounded, generation-local scheduler for all durable mux delivery classes.
///
/// `L` and `P` are noncoalescible lifecycle and payload values.  State and
/// render values are coalesced independently under their stable key types.
pub struct DeliveryScheduler<L, StateKey, StateValue, RenderKey, RenderValue, P> {
    limits: SchedulerLimits,
    durable_limit: usize,
    lifecycle: VecDeque<L>,
    state: KeyedLane<StateKey, StateValue>,
    render: KeyedLane<RenderKey, RenderValue>,
    payload: VecDeque<P>,
    wake_pending: bool,
    active_class: Option<DurableClass>,
    active_burst: usize,
    closed: bool,
    counters: SchedulerCounters,
}

impl<L, StateKey, StateValue, RenderKey, RenderValue, P>
    DeliveryScheduler<L, StateKey, StateValue, RenderKey, RenderValue, P>
where
    StateKey: Clone + Eq + Hash,
    RenderKey: Clone + Eq + Hash,
{
    /// Construct an empty scheduler, rejecting limits whose aggregate cannot
    /// be represented exactly.
    pub fn new(limits: SchedulerLimits) -> Result<Self, SchedulerConfigError> {
        let durable_limit = limits
            .durable_limit()
            .ok_or(SchedulerConfigError::CapacityOverflow)?;
        Ok(Self {
            limits,
            durable_limit,
            lifecycle: VecDeque::new(),
            state: KeyedLane::new(limits.state_keys),
            render: KeyedLane::new(limits.render_keys),
            payload: VecDeque::new(),
            wake_pending: false,
            active_class: None,
            active_burst: 0,
            closed: false,
            counters: SchedulerCounters::default(),
        })
    }

    #[must_use]
    pub const fn is_closed(&self) -> bool {
        self.closed
    }

    #[must_use]
    pub const fn counters(&self) -> SchedulerCounters {
        self.counters
    }

    /// Return exact logical capacity and current usage.
    #[must_use]
    pub fn capacity(&self) -> SchedulerCapacity {
        let lifecycle = LaneCapacity {
            limit: self.limits.lifecycle,
            used: self.lifecycle.len(),
        };
        let state_keys = LaneCapacity {
            limit: self.limits.state_keys,
            used: self.state.len(),
        };
        let state_resync = LaneCapacity {
            limit: 1,
            used: usize::from(self.state.resync_pending),
        };
        let render_keys = LaneCapacity {
            limit: self.limits.render_keys,
            used: self.render.len(),
        };
        let render_resync = LaneCapacity {
            limit: 1,
            used: usize::from(self.render.resync_pending),
        };
        let payload = LaneCapacity {
            limit: self.limits.payload,
            used: self.payload.len(),
        };
        let durable_used = lifecycle.used
            + state_keys.used
            + state_resync.used
            + render_keys.used
            + render_resync.used
            + payload.used;

        SchedulerCapacity {
            lifecycle,
            state_keys,
            state_resync,
            render_keys,
            render_resync,
            payload,
            durable_total: LaneCapacity {
                limit: self.durable_limit,
                used: durable_used,
            },
            wake: LaneCapacity {
                limit: 1,
                used: usize::from(self.wake_pending),
            },
        }
    }

    /// Pre-admit one strict-FIFO lifecycle barrier.
    pub fn admit_lifecycle(&mut self, value: L) -> Admission<L> {
        let admission = if self.closed {
            Admission::returned(AdmissionOutcome::Closed, value)
        } else if self.lifecycle.len() == self.limits.lifecycle {
            Admission::returned(AdmissionOutcome::Rejected, value)
        } else {
            self.lifecycle.push_back(value);
            Admission::accepted(AdmissionOutcome::Admitted)
        };
        self.finish_admission(DurableClass::Lifecycle, admission.outcome());
        admission
    }

    /// Publish the latest state value for a stable coalescing key.
    pub fn admit_state(
        &mut self,
        key: StateKey,
        value: StateValue,
    ) -> Admission<(StateKey, StateValue)> {
        let admission = if self.closed {
            Admission::returned(AdmissionOutcome::Closed, (key, value))
        } else {
            Admission::accepted(self.state.admit(key, value))
        };
        self.finish_admission(DurableClass::State, admission.outcome());
        admission
    }

    /// Publish the latest render value for a stable coalescing key.
    pub fn admit_render(
        &mut self,
        key: RenderKey,
        value: RenderValue,
    ) -> Admission<(RenderKey, RenderValue)> {
        let admission = if self.closed {
            Admission::returned(AdmissionOutcome::Closed, (key, value))
        } else {
            Admission::accepted(self.render.admit(key, value))
        };
        self.finish_admission(DurableClass::Render, admission.outcome());
        admission
    }

    /// Pre-admit one strict-FIFO true payload event.
    pub fn admit_payload(&mut self, value: P) -> Admission<P> {
        let admission = if self.closed {
            Admission::returned(AdmissionOutcome::Closed, value)
        } else if self.payload.len() == self.limits.payload {
            Admission::returned(AdmissionOutcome::Rejected, value)
        } else {
            self.payload.push_back(value);
            Admission::accepted(AdmissionOutcome::Admitted)
        };
        self.finish_admission(DurableClass::Payload, admission.outcome());
        admission
    }

    /// Publish a payload-free best-effort wake edge.
    pub fn request_wake(&mut self) -> WakeOutcome {
        if self.closed {
            increment(&mut self.counters.wake.closed);
            WakeOutcome::Closed
        } else {
            self.arm_wake()
        }
    }

    /// Consume the coalesced wake hint.  Durable obligations are unchanged.
    pub fn take_wake(&mut self) -> bool {
        if self.wake_pending {
            self.wake_pending = false;
            increment(&mut self.counters.wake.consumed);
            true
        } else {
            false
        }
    }

    /// Schedule one durable item according to bounded cross-class priority.
    pub fn pop_next(
        &mut self,
    ) -> Option<ScheduledItem<L, StateKey, StateValue, RenderKey, RenderValue, P>> {
        if self.closed {
            return None;
        }

        let class = self.select_class()?;
        let item = match class {
            DurableClass::Lifecycle => {
                ScheduledItem::Lifecycle(self.lifecycle.pop_front().expect("ready lifecycle lane"))
            }
            DurableClass::State => match self.state.pop().expect("ready state lane") {
                KeyedPop::Value { key, value } => ScheduledItem::State { key, value },
                KeyedPop::Resync => ScheduledItem::StateResync,
            },
            DurableClass::Render => match self.render.pop().expect("ready render lane") {
                KeyedPop::Value { key, value } => ScheduledItem::Render { key, value },
                KeyedPop::Resync => ScheduledItem::RenderResync,
            },
            DurableClass::Payload => {
                ScheduledItem::Payload(self.payload.pop_front().expect("ready payload lane"))
            }
        };
        increment(&mut self.class_counters_mut(class).dequeued);
        Some(item)
    }

    /// Terminally close this local generation.
    ///
    /// All pending local obligations become terminal rather than falsely
    /// settled.  Reconnect/bootstrap must create a fresh scheduler and
    /// authoritative state.
    pub fn close(&mut self) -> SchedulerCloseOutcome {
        if self.closed {
            return SchedulerCloseOutcome::AlreadyClosed;
        }

        let discarded_obligations = self.capacity().durable_total.used;
        self.lifecycle.clear();
        self.state.clear();
        self.render.clear();
        self.payload.clear();
        self.wake_pending = false;
        self.active_class = None;
        self.active_burst = 0;
        self.closed = true;
        increment(&mut self.counters.shutdowns);
        add_usize(
            &mut self.counters.discarded_on_shutdown,
            discarded_obligations,
        );
        SchedulerCloseOutcome::Closed {
            discarded_obligations,
        }
    }

    /// Validate all structural bounds and bookkeeping relationships.
    pub fn check_invariants(&self) -> Result<(), &'static str> {
        if self.lifecycle.len() > self.limits.lifecycle {
            return Err("lifecycle lane exceeds its configured limit");
        }
        if self.payload.len() > self.limits.payload {
            return Err("payload lane exceeds its configured limit");
        }
        self.state.check_invariants()?;
        self.render.check_invariants()?;

        let capacity = self.capacity();
        let component_sum = capacity.lifecycle.used
            + capacity.state_keys.used
            + capacity.state_resync.used
            + capacity.render_keys.used
            + capacity.render_resync.used
            + capacity.payload.used;
        if component_sum != capacity.durable_total.used {
            return Err("aggregate durable usage disagrees with component usage");
        }
        if capacity.durable_total.used > capacity.durable_total.limit {
            return Err("aggregate durable usage exceeds its exact limit");
        }
        if self.active_burst > DELIVERY_PRIORITY_BURST_LIMIT {
            return Err("active priority burst exceeds the fairness limit");
        }
        if self.active_class.is_none() && self.active_burst != 0 {
            return Err("inactive priority cursor retains a nonzero burst");
        }
        if self.closed
            && (capacity.durable_total.used != 0
                || capacity.wake.used != 0
                || self.active_class.is_some())
        {
            return Err("closed scheduler retains live local obligations");
        }
        Ok(())
    }

    fn finish_admission(&mut self, class: DurableClass, outcome: AdmissionOutcome) {
        self.class_counters_mut(class).record_admission(outcome);
        if matches!(
            outcome,
            AdmissionOutcome::Admitted | AdmissionOutcome::Coalesced | AdmissionOutcome::Escalated
        ) {
            let _ = self.arm_wake();
        }
    }

    fn arm_wake(&mut self) -> WakeOutcome {
        if self.wake_pending {
            increment(&mut self.counters.wake.coalesced);
            WakeOutcome::Coalesced
        } else {
            self.wake_pending = true;
            increment(&mut self.counters.wake.admitted);
            WakeOutcome::Admitted
        }
    }

    fn class_counters_mut(&mut self, class: DurableClass) -> &mut ClassCounters {
        match class {
            DurableClass::Lifecycle => &mut self.counters.lifecycle,
            DurableClass::State => &mut self.counters.state,
            DurableClass::Render => &mut self.counters.render,
            DurableClass::Payload => &mut self.counters.payload,
        }
    }

    fn class_ready(&self, class: DurableClass) -> bool {
        match class {
            DurableClass::Lifecycle => !self.lifecycle.is_empty(),
            DurableClass::State => self.state.is_ready(),
            DurableClass::Render => self.render.is_ready(),
            DurableClass::Payload => !self.payload.is_empty(),
        }
    }

    fn select_class(&mut self) -> Option<DurableClass> {
        if let Some(active) = self.active_class
            && self.active_burst < DELIVERY_PRIORITY_BURST_LIMIT
            && self.class_ready(active)
        {
            self.active_burst += 1;
            return Some(active);
        }

        let start = self.active_class.map_or(0, |class| {
            (class.index() + 1) % DELIVERY_DURABLE_CLASS_COUNT
        });
        for offset in 0..DELIVERY_DURABLE_CLASS_COUNT {
            let class = DURABLE_CLASSES[(start + offset) % DELIVERY_DURABLE_CLASS_COUNT];
            if self.class_ready(class) {
                self.active_class = Some(class);
                self.active_burst = 1;
                return Some(class);
            }
        }

        self.active_class = None;
        self.active_burst = 0;
        None
    }
}

/// Stable identifier for the acknowledgement-retaining scheduler contract.
pub const HARDENED_DELIVERY_SCHEDULER_CONTRACT_VERSION: &str = "ft.delivery-coordinator.v2";

/// Number of independently charged durable authority lanes.
pub const HARDENED_DELIVERY_LANE_COUNT: usize = 8;

/// Fixed-size authority lane used by an exhaustive [`PlannedEffect`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(usize)]
pub enum AuthorityLane {
    Lifecycle = 0,
    State = 1,
    Render = 2,
    Topology = 3,
    Auxiliary = 4,
    Event = 5,
    Telemetry = 6,
    Spool = 7,
}

impl AuthorityLane {
    #[must_use]
    pub const fn index(self) -> usize {
        self as usize
    }
}

/// Exact scheduler generation for durable authority handles.
///
/// The public numeric ordinal is useful for observability and wire settlement,
/// but cannot establish authority by itself: it may repeat after a client
/// restart. `connection_identity` binds the generation to the coherent
/// topology bootstrap, while the private process-local `instance` prevents a
/// replacement coordinator on the same connection from accepting a cloned
/// plan minted for its predecessor.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SchedulerGeneration {
    connection_identity: RenderConnectionIdentity,
    ordinal: u64,
    instance: SchedulerGenerationInstanceId,
}

impl SchedulerGeneration {
    #[cfg(test)]
    const fn for_test(
        connection_identity: RenderConnectionIdentity,
        ordinal: u64,
        instance: u64,
    ) -> Self {
        Self {
            connection_identity,
            ordinal,
            instance: SchedulerGenerationInstanceId(instance),
        }
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.ordinal
    }

    #[must_use]
    pub const fn connection_identity(self) -> RenderConnectionIdentity {
        self.connection_identity
    }

    #[must_use]
    pub const fn instance_id(self) -> SchedulerGenerationInstanceId {
        self.instance
    }
}

/// Opaque process-local identity for one scheduler-generation authority.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SchedulerGenerationInstanceId(u64);

impl SchedulerGenerationInstanceId {
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SchedulerGenerationError {
    InvalidConnectionIdentity,
    ZeroOrdinal,
    InstanceExhausted,
}

impl std::fmt::Display for SchedulerGenerationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::InvalidConnectionIdentity => {
                "scheduler generation requires a non-reserved render connection identity"
            }
            Self::ZeroOrdinal => "scheduler generation ordinal must be nonzero",
            Self::InstanceExhausted => {
                "scheduler generation instance space is exhausted; refusing to wrap or reuse"
            }
        })
    }
}

impl std::error::Error for SchedulerGenerationError {}

static NEXT_SCHEDULER_GENERATION_INSTANCE: AtomicU64 = AtomicU64::new(1);

fn allocate_scheduler_generation_instance(
    counter: &AtomicU64,
) -> Result<SchedulerGenerationInstanceId, SchedulerGenerationError> {
    let mut current = counter.load(AtomicOrdering::Relaxed);
    loop {
        if current == 0 {
            return Err(SchedulerGenerationError::InstanceExhausted);
        }
        let next = current
            .checked_add(1)
            .ok_or(SchedulerGenerationError::InstanceExhausted)?;
        match counter.compare_exchange_weak(
            current,
            next,
            AtomicOrdering::Relaxed,
            AtomicOrdering::Relaxed,
        ) {
            Ok(_) => return Ok(SchedulerGenerationInstanceId(current)),
            Err(observed) => current = observed,
        }
    }
}

fn mint_scheduler_generation(
    connection_identity: RenderConnectionIdentity,
    ordinal: u64,
) -> Result<SchedulerGeneration, SchedulerGenerationError> {
    connection_identity
        .validate()
        .map_err(|_| SchedulerGenerationError::InvalidConnectionIdentity)?;
    if ordinal == 0 {
        return Err(SchedulerGenerationError::ZeroOrdinal);
    }
    let instance =
        allocate_scheduler_generation_instance(&NEXT_SCHEDULER_GENERATION_INSTANCE)?;
    Ok(SchedulerGeneration {
        connection_identity,
        ordinal,
        instance,
    })
}

/// Stable key plus incarnation.  Raw pane IDs are insufficient when an ID can
/// be reused across topology epochs.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct StableEffectKey {
    scope: u64,
    incarnation: u64,
}

impl StableEffectKey {
    #[must_use]
    pub const fn new(scope: u64, incarnation: u64) -> Self {
        Self { scope, incarnation }
    }

    #[must_use]
    pub const fn scope(self) -> u64 {
        self.scope
    }

    #[must_use]
    pub const fn incarnation(self) -> u64 {
        self.incarnation
    }
}

/// Fixed-size reference to state owned by a durable journal, snapshot store,
/// fold accumulator, spool, or external render ledger.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct AuthorityHandle {
    generation: SchedulerGeneration,
    id: u64,
    version: u64,
}

impl AuthorityHandle {
    #[must_use]
    pub const fn new(generation: SchedulerGeneration, id: u64, version: u64) -> Self {
        Self {
            generation,
            id,
            version,
        }
    }

    #[must_use]
    pub const fn generation(self) -> SchedulerGeneration {
        self.generation
    }

    #[must_use]
    pub const fn id(self) -> u64 {
        self.id
    }

    #[must_use]
    pub const fn version(self) -> u64 {
        self.version
    }
}

/// Versioned authoritative replacement snapshot.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ResyncAuthority {
    handle: AuthorityHandle,
    source_version: u64,
}

impl ResyncAuthority {
    #[must_use]
    pub const fn new(handle: AuthorityHandle, source_version: u64) -> Self {
        Self {
            handle,
            source_version,
        }
    }

    #[must_use]
    pub const fn handle(self) -> AuthorityHandle {
        self.handle
    }

    #[must_use]
    pub const fn source_version(self) -> u64 {
        self.source_version
    }
}

/// Claim or resync authority retained by the external render ledger.  The
/// scheduler arbitrates this fixed-size handle and never independently
/// coalesces pane dirties or synthesizes its own render replacement snapshot.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ExternalRenderAuthority {
    scheduler_generation: SchedulerGeneration,
    ledger_instance: DeliveryLedgerInstance,
    ledger_generation: u64,
    ledger_obligation: u64,
    ledger_scope: DeliveryScope,
    source_version: u64,
}

impl ExternalRenderAuthority {
    #[must_use]
    pub const fn from_delivery_claim(
        scheduler_generation: SchedulerGeneration,
        claim: DeliveryClaim,
        source_version: u64,
    ) -> Self {
        let token = claim.token();
        Self {
            scheduler_generation,
            ledger_instance: token.instance(),
            ledger_generation: token.generation().get(),
            ledger_obligation: token.sequence(),
            ledger_scope: claim.scope(),
            source_version,
        }
    }

    #[cfg(test)]
    const fn new_for_test(
        scheduler_generation: SchedulerGeneration,
        ledger_instance: u64,
        ledger_generation: u64,
        ledger_obligation: u64,
        ledger_scope: DeliveryScope,
        source_version: u64,
    ) -> Self {
        Self {
            scheduler_generation,
            ledger_instance: DeliveryLedgerInstance::for_test(ledger_instance),
            ledger_generation,
            ledger_obligation,
            ledger_scope,
            source_version,
        }
    }

    #[must_use]
    pub const fn scheduler_generation(self) -> SchedulerGeneration {
        self.scheduler_generation
    }

    #[must_use]
    pub const fn ledger_instance(self) -> DeliveryLedgerInstance {
        self.ledger_instance
    }

    #[must_use]
    pub const fn ledger_generation(self) -> u64 {
        self.ledger_generation
    }

    #[must_use]
    pub const fn ledger_obligation(self) -> u64 {
        self.ledger_obligation
    }

    #[must_use]
    pub const fn ledger_scope(self) -> DeliveryScope {
        self.ledger_scope
    }

    #[must_use]
    pub const fn source_version(self) -> u64 {
        self.source_version
    }
}

/// Scope of a lifecycle barrier.  V2 schedules every scope conservatively as a
/// global FIFO fence; retaining the scope prevents a later live implementation
/// from guessing it after admission.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum BarrierScope {
    Global,
    Key(StableEffectKey),
}

/// Exact fold authority that must precede a render claim.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TelemetryFence {
    accumulator: AuthorityHandle,
    through_version: u64,
}

impl TelemetryFence {
    #[must_use]
    pub const fn new(accumulator: AuthorityHandle, through_version: u64) -> Self {
        Self {
            accumulator,
            through_version,
        }
    }

    #[must_use]
    pub const fn accumulator(self) -> AuthorityHandle {
        self.accumulator
    }

    #[must_use]
    pub const fn through_version(self) -> u64 {
        self.through_version
    }
}

/// Closed, exhaustive plan effect.  No variant stores an unbounded payload;
/// externally sized data is represented by a journal or spool handle.
///
/// Each `retained_bytes` field is the additional authority-owned resident
/// memory kept alive by that handle.  The coordinator independently adds the
/// exact structural charge for its queue entry and effect allocation, so a
/// zero additional charge can never make a stored plan free.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlannedEffect {
    LifecycleBarrier {
        scope: BarrierScope,
        journal: AuthorityHandle,
        retained_bytes: usize,
    },
    StateAuthority {
        key: StableEffectKey,
        authority: AuthorityHandle,
        retained_bytes: usize,
    },
    StateResync {
        authority: ResyncAuthority,
        retained_bytes: usize,
    },
    RenderAuthority {
        key: StableEffectKey,
        claim: ExternalRenderAuthority,
        telemetry_through: Option<TelemetryFence>,
        retained_bytes: usize,
    },
    RenderResync {
        authority: ExternalRenderAuthority,
        retained_bytes: usize,
    },
    TopologyResync {
        authority: ResyncAuthority,
        retained_bytes: usize,
    },
    AuxiliaryResync {
        authority: ResyncAuthority,
        retained_bytes: usize,
    },
    EventJournal {
        journal: AuthorityHandle,
        retained_bytes: usize,
    },
    TelemetryFold {
        key: StableEffectKey,
        accumulator: AuthorityHandle,
        through_version: u64,
        retained_bytes: usize,
    },
    Spool {
        spool: AuthorityHandle,
        payload_bytes: u64,
        retained_bytes: usize,
    },
}

impl PlannedEffect {
    #[must_use]
    pub const fn lane(&self) -> AuthorityLane {
        match self {
            Self::LifecycleBarrier { .. } => AuthorityLane::Lifecycle,
            Self::StateAuthority { .. } | Self::StateResync { .. } => AuthorityLane::State,
            Self::RenderAuthority { .. } | Self::RenderResync { .. } => AuthorityLane::Render,
            Self::TopologyResync { .. } => AuthorityLane::Topology,
            Self::AuxiliaryResync { .. } => AuthorityLane::Auxiliary,
            Self::EventJournal { .. } => AuthorityLane::Event,
            Self::TelemetryFold { .. } => AuthorityLane::Telemetry,
            Self::Spool { .. } => AuthorityLane::Spool,
        }
    }

    #[must_use]
    pub const fn retained_bytes(&self) -> usize {
        let bytes = match self {
            Self::LifecycleBarrier { retained_bytes, .. }
            | Self::StateAuthority { retained_bytes, .. }
            | Self::StateResync { retained_bytes, .. }
            | Self::RenderAuthority { retained_bytes, .. }
            | Self::RenderResync { retained_bytes, .. }
            | Self::TopologyResync { retained_bytes, .. }
            | Self::AuxiliaryResync { retained_bytes, .. }
            | Self::EventJournal { retained_bytes, .. }
            | Self::TelemetryFold { retained_bytes, .. }
            | Self::Spool { retained_bytes, .. } => *retained_bytes,
        };
        bytes
    }

    const fn generation(&self) -> SchedulerGeneration {
        match self {
            Self::LifecycleBarrier { journal, .. }
            | Self::StateAuthority {
                authority: journal, ..
            }
            | Self::EventJournal { journal, .. }
            | Self::TelemetryFold {
                accumulator: journal,
                ..
            }
            | Self::Spool { spool: journal, .. } => journal.generation(),
            Self::StateResync { authority, .. }
            | Self::TopologyResync { authority, .. }
            | Self::AuxiliaryResync { authority, .. } => authority.handle().generation(),
            Self::RenderAuthority { claim, .. } => claim.scheduler_generation(),
            Self::RenderResync { authority, .. } => authority.scheduler_generation(),
        }
    }

    const fn is_barrier(&self) -> bool {
        matches!(self, Self::LifecycleBarrier { .. })
    }
}

/// Atomic, ordered group of scheduler-owned authority handles.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeliveryPlan {
    effects: Arc<[PlannedEffect]>,
}

const SHARED_PLAN_ALLOCATION_HEADER_BYTES: usize = 2 * std::mem::size_of::<usize>();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlanBuildError {
    Empty,
    FootprintOverflow,
}

impl DeliveryPlan {
    pub fn new(effects: Vec<PlannedEffect>) -> Result<Self, PlanBuildError> {
        if effects.is_empty() {
            return Err(PlanBuildError::Empty);
        }
        let plan = Self {
            effects: Arc::from(effects.into_boxed_slice()),
        };
        plan.footprint().ok_or(PlanBuildError::FootprintOverflow)?;
        Ok(plan)
    }

    #[must_use]
    pub fn effects(&self) -> &[PlannedEffect] {
        self.effects.as_ref()
    }

    fn footprint(&self) -> Option<PlanFootprint> {
        let mut lane_slots = [0usize; HARDENED_DELIVERY_LANE_COUNT];
        let effect_storage =
            std::mem::size_of::<PlannedEffect>().checked_mul(self.effects.len())?;
        let mut resident_bytes = std::mem::size_of::<CoordinatorEntry>()
            .checked_add(SHARED_PLAN_ALLOCATION_HEADER_BYTES)?
            .checked_add(effect_storage)?;
        let mut barrier_count = 0usize;
        for effect in self.effects.as_ref() {
            let lane = &mut lane_slots[effect.lane().index()];
            *lane = lane.checked_add(1)?;
            resident_bytes = resident_bytes.checked_add(effect.retained_bytes())?;
            barrier_count = barrier_count.checked_add(usize::from(effect.is_barrier()))?;
        }
        Some(PlanFootprint {
            lane_slots,
            semantic_effects: self.effects.len(),
            resident_bytes,
            barrier_count,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PlanFootprint {
    lane_slots: [usize; HARDENED_DELIVERY_LANE_COUNT],
    semantic_effects: usize,
    resident_bytes: usize,
    barrier_count: usize,
}

impl PlanFootprint {
    fn total_slots(self) -> Option<usize> {
        self.lane_slots
            .into_iter()
            .try_fold(0usize, usize::checked_add)
    }
}

/// Checked slot and byte limits for v2 authority handles.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HardenedSchedulerLimits {
    lane_slots: [usize; HARDENED_DELIVERY_LANE_COUNT],
    total_slots: usize,
    resident_bytes: usize,
    max_effects_per_plan: usize,
    max_effect_resident_bytes: usize,
    pending_plans: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HardenedConfigError {
    CapacityOverflow,
    ZeroPlanLimit,
    ZeroEffectsPerPlan,
}

impl HardenedSchedulerLimits {
    pub fn new(
        lane_slots: [usize; HARDENED_DELIVERY_LANE_COUNT],
        total_slots: usize,
        resident_bytes: usize,
        max_effects_per_plan: usize,
        max_effect_resident_bytes: usize,
        pending_plans: usize,
    ) -> Result<Self, HardenedConfigError> {
        let aggregate = lane_slots
            .into_iter()
            .try_fold(0usize, usize::checked_add)
            .ok_or(HardenedConfigError::CapacityOverflow)?;
        if total_slots > aggregate {
            return Err(HardenedConfigError::CapacityOverflow);
        }
        if pending_plans == 0 {
            return Err(HardenedConfigError::ZeroPlanLimit);
        }
        if max_effects_per_plan == 0 {
            return Err(HardenedConfigError::ZeroEffectsPerPlan);
        }
        Ok(Self {
            lane_slots,
            total_slots,
            resident_bytes,
            max_effects_per_plan,
            max_effect_resident_bytes,
            pending_plans,
        })
    }

    #[must_use]
    pub const fn lane_limit(self, lane: AuthorityLane) -> usize {
        self.lane_slots[lane.index()]
    }

    #[must_use]
    pub const fn total_slots(self) -> usize {
        self.total_slots
    }

    #[must_use]
    pub const fn resident_bytes(self) -> usize {
        self.resident_bytes
    }
}

/// Exact usage partition.  Claiming moves a plan from ready to in-flight and
/// does not release any slot or byte charge.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HardenedCapacity {
    pub lane_slots_used: [usize; HARDENED_DELIVERY_LANE_COUNT],
    pub charged_slots: usize,
    pub charged_resident_bytes: usize,
    pub semantic_effects: usize,
    pub reserved_plans: usize,
    pub ready_plans: usize,
    pub in_flight_plans: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HardenedCounters {
    pub plans_admitted: u64,
    pub plans_rejected: u64,
    pub reservations_admitted: u64,
    pub reservations_rejected: u64,
    pub reservations_committed: u64,
    pub reservations_cancelled: u64,
    pub reservation_cancel_rejections: u64,
    pub reservation_commit_rejections: u64,
    pub stale_reservations: u64,
    pub wrong_instance_reservations: u64,
    pub claims: u64,
    pub acknowledgements: u64,
    pub retries: u64,
    pub terminal_nacks: u64,
    pub stale_settlements: u64,
    pub wrong_instance_settlements: u64,
    pub wake_registrations: u64,
    /// Registered-consumer wakes extracted for delivery by the owner.
    pub wake_deliveries: u64,
    pub closes: u64,
    pub closed_semantic_effects: u64,
    pub closed_charged_slots: u64,
    pub closed_resident_bytes: u64,
}

static NEXT_COORDINATOR_INSTANCE: AtomicU64 = AtomicU64::new(1);

/// Process-local, never-reused identity for one delivery coordinator.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct CoordinatorInstanceId(u64);

impl CoordinatorInstanceId {
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CoordinatorInstanceExhausted;

impl std::fmt::Display for CoordinatorInstanceExhausted {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(
            "delivery-coordinator instance identity space is exhausted; refusing to wrap or reuse",
        )
    }
}

impl std::error::Error for CoordinatorInstanceExhausted {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeliveryCoordinatorCreateError {
    SchedulerGeneration(SchedulerGenerationError),
    CoordinatorInstance(CoordinatorInstanceExhausted),
}

impl std::fmt::Display for DeliveryCoordinatorCreateError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SchedulerGeneration(error) => error.fmt(formatter),
            Self::CoordinatorInstance(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for DeliveryCoordinatorCreateError {}

impl From<SchedulerGenerationError> for DeliveryCoordinatorCreateError {
    fn from(error: SchedulerGenerationError) -> Self {
        Self::SchedulerGeneration(error)
    }
}

impl From<CoordinatorInstanceExhausted> for DeliveryCoordinatorCreateError {
    fn from(error: CoordinatorInstanceExhausted) -> Self {
        Self::CoordinatorInstance(error)
    }
}

fn allocate_coordinator_instance(
    counter: &AtomicU64,
) -> Result<CoordinatorInstanceId, CoordinatorInstanceExhausted> {
    let mut current = counter.load(AtomicOrdering::Relaxed);
    loop {
        if current == 0 {
            return Err(CoordinatorInstanceExhausted);
        }
        let next = current
            .checked_add(1)
            .ok_or(CoordinatorInstanceExhausted)?;
        match counter.compare_exchange_weak(
            current,
            next,
            AtomicOrdering::Relaxed,
            AtomicOrdering::Relaxed,
        ) {
            Ok(_) => return Ok(CoordinatorInstanceId(current)),
            Err(observed) => current = observed,
        }
    }
}

#[derive(Clone, Debug)]
struct CoordinatorInstance {
    id: CoordinatorInstanceId,
    marker: Arc<()>,
}

impl CoordinatorInstance {
    fn try_new() -> Result<Self, CoordinatorInstanceExhausted> {
        Ok(Self {
            id: allocate_coordinator_instance(&NEXT_COORDINATOR_INSTANCE)?,
            marker: Arc::new(()),
        })
    }

    const fn id(&self) -> CoordinatorInstanceId {
        self.id
    }
}

impl PartialEq for CoordinatorInstance {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && Arc::ptr_eq(&self.marker, &other.marker)
    }
}

impl Eq for CoordinatorInstance {}

/// Opaque, instance-bound reservation capability.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReservationToken {
    instance: CoordinatorInstance,
    generation: SchedulerGeneration,
    sequence: u64,
}

impl ReservationToken {
    #[must_use]
    pub const fn generation(&self) -> SchedulerGeneration {
        self.generation
    }

    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }
}

/// Opaque, instance-bound application-attempt capability.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CoordinatorClaimToken {
    instance: CoordinatorInstance,
    generation: SchedulerGeneration,
    obligation_sequence: u64,
    attempt: u64,
}

impl CoordinatorClaimToken {
    #[must_use]
    pub const fn coordinator_instance(&self) -> CoordinatorInstanceId {
        self.instance.id()
    }

    #[must_use]
    pub const fn generation(&self) -> SchedulerGeneration {
        self.generation
    }

    #[must_use]
    pub const fn obligation_sequence(&self) -> u64 {
        self.obligation_sequence
    }

    #[must_use]
    pub const fn attempt(&self) -> u64 {
        self.attempt
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CoordinatorDeliveryClaim {
    token: CoordinatorClaimToken,
    epoch: u64,
    plan: DeliveryPlan,
}

impl CoordinatorDeliveryClaim {
    #[must_use]
    pub const fn token(&self) -> &CoordinatorClaimToken {
        &self.token
    }

    #[must_use]
    pub const fn epoch(&self) -> u64 {
        self.epoch
    }

    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.token.obligation_sequence
    }

    #[must_use]
    pub fn plan(&self) -> &DeliveryPlan {
        &self.plan
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlanRejectReason {
    Capacity,
    ResidentBytes,
    EffectTooLarge,
    TooManyEffects,
    MultipleBarriers,
    BarrierMustBeIsolated,
    WrongGeneration,
    RenderScopeMismatch,
    TelemetryDependencyOrder,
    FootprintMismatch,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[must_use = "plan admission must be handled so rejected or closed plans retain explicit disposition"]
pub enum PlanAdmission {
    Admitted {
        sequence: u64,
        epoch: u64,
    },
    Rejected {
        reason: PlanRejectReason,
        plan: DeliveryPlan,
    },
    Closed {
        plan: DeliveryPlan,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[must_use = "reservation admission must be handled or a charged FIFO reservation can be orphaned"]
pub enum ReservationAdmission {
    Reserved(ReservationToken),
    Rejected(PlanRejectReason),
    Closed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[must_use = "reservation commit must be handled so rejected, stale, or wrong-instance work is not lost"]
pub enum ReservationCommit {
    Committed {
        sequence: u64,
        epoch: u64,
    },
    Rejected {
        token: ReservationToken,
        reason: PlanRejectReason,
        plan: DeliveryPlan,
    },
    Stale {
        plan: DeliveryPlan,
    },
    WrongInstance {
        token: ReservationToken,
        plan: DeliveryPlan,
    },
    Closed {
        plan: DeliveryPlan,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[must_use = "reservation cancellation must be handled so causal dependents and stale ownership are explicit"]
pub enum ReservationCancel {
    Cancelled,
    CausalDependents { token: ReservationToken },
    Stale,
    WrongInstance { token: ReservationToken },
    GenerationClosed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use = "coordinator settlement must be handled so retries and failed-closed outcomes remain explicit"]
pub enum CoordinatorSettleOutcome {
    Acknowledged,
    Retried,
    FailedClosed,
    StaleOrDuplicate,
    WrongInstance,
    GenerationClosed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CoordinatorClaimError {
    TokenExhausted,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CoordinatorCloseReport {
    pub reserved_plans: usize,
    pub ready_plans: usize,
    pub in_flight_plans: usize,
    pub semantic_effects: usize,
    pub charged_slots: usize,
    pub charged_resident_bytes: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CoordinatorCloseOutcome {
    Closed(CoordinatorCloseReport),
    AlreadyClosed,
}

/// Stable identifier for the exact post-application settlement model.
pub const RENDER_APPLICATION_SETTLEMENT_CONTRACT_VERSION: &str =
    "ft.render-application-settlement.v1";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RenderApplicationSettlementCounters {
    pub applications_begun: u64,
    pub begin_rejections: u64,
    pub acknowledgements: u64,
    pub retries: u64,
    pub resync_requests: u64,
    pub terminal_nacks: u64,
    pub retry_exhaustions: u64,
    pub deadline_expirations: u64,
    pub stale_or_invalid_results: u64,
    pub closes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RenderApplicationClaimMismatch {
    CoordinatorInstance,
    ConnectionIdentity,
    ConnectionGeneration,
    SchedulerSequence,
    Attempt,
    MissingRenderEffect,
    MultipleRenderEffects,
    LedgerInstance,
    RenderGeneration,
    LedgerObligation,
    Pane,
    Kind,
    SourceVersion,
    GlobalResyncRequiresManifest,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use = "a rejected render application must leave the coordinator claim explicitly owned"]
pub enum RenderApplicationBeginError {
    Contract(RenderApplicationContractError),
    TrackerClosed,
    ApplicationAlreadyPending,
    ClaimMismatch(RenderApplicationClaimMismatch),
    RetryMustStartAtOne,
    RetryIdentityMismatch,
    RetryOrdinalMismatch,
    RetryDeadlineExpired,
    RetryDeadlineExtended,
    DeadlineOverflow,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use = "render application settlement must be handled so commit, retry, resync, and terminal outcomes stay explicit"]
pub enum RenderApplicationSettleOutcome {
    Acknowledged,
    Retried,
    AuthoritativeResyncScheduled {
        reason: RenderApplicationNackReason,
    },
    FailedClosed {
        reason: RenderApplicationNackReason,
        coordinator: CoordinatorSettleOutcome,
    },
    RetryExhausted {
        reason: RenderApplicationNackReason,
        coordinator: CoordinatorSettleOutcome,
    },
    DeadlineExpired {
        coordinator: CoordinatorSettleOutcome,
    },
    StaleOrDuplicate,
    Rejected(RenderApplicationContractError),
    CoordinatorDiverged(CoordinatorSettleOutcome),
    TrackerClosed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RenderApplicationCloseOutcome {
    Closed(CoordinatorCloseOutcome),
    AlreadyClosed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RenderRetryIdentity {
    connection_identity: RenderConnectionIdentity,
    connection_generation: u64,
    coordinator_instance: u64,
    scheduler_sequence: u64,
    ledger_instance: u64,
    render_generation: u64,
    ledger_obligation: u64,
    pane_id: u64,
    base_state: Option<codec::RenderStateIdentity>,
    resulting_state: codec::RenderStateIdentity,
    kind: RenderApplicationKind,
}

impl RenderRetryIdentity {
    fn from_identity(
        connection_identity: RenderConnectionIdentity,
        identity: RenderApplicationIdentity,
    ) -> Result<Self, RenderApplicationBeginError> {
        Ok(Self {
            connection_identity,
            connection_generation: identity.token.connection_generation,
            coordinator_instance: identity.token.coordinator_instance,
            scheduler_sequence: identity.token.scheduler_sequence,
            ledger_instance: identity.token.ledger_instance,
            render_generation: identity.token.render_generation,
            ledger_obligation: identity.token.ledger_obligation,
            pane_id: u64::try_from(identity.pane_id)
                .map_err(|_| RenderApplicationBeginError::RetryIdentityMismatch)?,
            base_state: identity.base_state,
            resulting_state: identity.resulting_state,
            kind: identity.kind,
        })
    }

    fn authoritative_resync(
        connection_identity: RenderConnectionIdentity,
        identity: RenderApplicationIdentity,
    ) -> Result<Self, RenderApplicationBeginError> {
        let mut retry = Self::from_identity(connection_identity, identity)?;
        retry.base_state = None;
        retry.kind = RenderApplicationKind::Snapshot;
        Ok(retry)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RenderRetryContext {
    identity: RenderRetryIdentity,
    max_attempts: u16,
    last_attempt_ordinal: u16,
    deadline_millis: u64,
}

#[derive(Clone, Debug)]
struct PendingRenderApplication {
    connection_identity: RenderConnectionIdentity,
    identity: RenderApplicationIdentity,
    claim: CoordinatorClaimToken,
    max_attempts: u16,
    attempt_ordinal: u16,
    deadline_millis: u64,
}

/// Fixed-memory settlement authority for one coordinator consumer.
///
/// The hardened coordinator permits one in-flight plan, so this tracker stores
/// at most one exact application identity and one fixed-size retry context. It
/// never retains the render payload. A stale, duplicate, wrong-pane,
/// wrong-generation, or wrong-attempt result cannot reach coordinator
/// settlement. A resync-class NACK atomically returns the exact authority to
/// the coordinator's ready lane and fences its next attempt to an authoritative
/// snapshot, so there is no release-before-replacement gap.
#[derive(Default)]
pub struct RenderApplicationSettlementTracker {
    pending: Option<PendingRenderApplication>,
    retry: Option<RenderRetryContext>,
    closed: bool,
    counters: RenderApplicationSettlementCounters,
}

impl RenderApplicationSettlementTracker {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub const fn counters(&self) -> RenderApplicationSettlementCounters {
        self.counters
    }

    #[must_use]
    pub const fn pending_identity(&self) -> Option<RenderApplicationIdentity> {
        match &self.pending {
            Some(pending) => Some(pending.identity),
            None => None,
        }
    }

    /// Return the fixed monotonic deadline for pending or ready-to-retry work.
    #[must_use]
    pub const fn deadline_millis(&self) -> Option<u64> {
        match &self.pending {
            Some(pending) => Some(pending.deadline_millis),
            None => match &self.retry {
                Some(retry) => Some(retry.deadline_millis),
                None => None,
            },
        }
    }

    #[must_use]
    pub const fn is_closed(&self) -> bool {
        self.closed
    }

    /// Bind an application payload to the exact coordinator and ledger claim.
    ///
    /// `now_millis` is a connection-owner monotonic timestamp. The advertised
    /// remaining duration may shorten an existing retry deadline but can never
    /// extend it.
    pub fn begin(
        &mut self,
        claim: &CoordinatorDeliveryClaim,
        update: &RenderApplicationUpdate,
        now_millis: u64,
    ) -> Result<(), RenderApplicationBeginError> {
        let result = self.begin_inner(claim, update, now_millis);
        if result.is_err() {
            increment(&mut self.counters.begin_rejections);
        } else {
            increment(&mut self.counters.applications_begun);
        }
        result
    }

    fn begin_inner(
        &mut self,
        claim: &CoordinatorDeliveryClaim,
        update: &RenderApplicationUpdate,
        now_millis: u64,
    ) -> Result<(), RenderApplicationBeginError> {
        if self.closed {
            return Err(RenderApplicationBeginError::TrackerClosed);
        }
        if self.pending.is_some() {
            return Err(RenderApplicationBeginError::ApplicationAlreadyPending);
        }
        update
            .validate()
            .map_err(RenderApplicationBeginError::Contract)?;
        validate_render_claim_binding(
            claim,
            update,
            self.retry.map(|retry| retry.identity.kind),
        )?;

        let advertised_deadline = now_millis
            .checked_add(u64::from(update.retry_budget.remaining_millis))
            .ok_or(RenderApplicationBeginError::DeadlineOverflow)?;
        let retry_identity =
            RenderRetryIdentity::from_identity(update.connection_identity, update.identity)?;
        let deadline_millis = match self.retry {
            Some(retry) => {
                if retry.identity != retry_identity
                    || retry.max_attempts != update.retry_budget.max_attempts
                {
                    return Err(RenderApplicationBeginError::RetryIdentityMismatch);
                }
                if now_millis >= retry.deadline_millis {
                    return Err(RenderApplicationBeginError::RetryDeadlineExpired);
                }
                let expected_ordinal = retry
                    .last_attempt_ordinal
                    .checked_add(1)
                    .ok_or(RenderApplicationBeginError::RetryOrdinalMismatch)?;
                if update.retry_budget.attempt_ordinal != expected_ordinal {
                    return Err(RenderApplicationBeginError::RetryOrdinalMismatch);
                }
                if advertised_deadline > retry.deadline_millis {
                    return Err(RenderApplicationBeginError::RetryDeadlineExtended);
                }
                retry.deadline_millis
            }
            None => {
                if update.retry_budget.attempt_ordinal != 1 {
                    return Err(RenderApplicationBeginError::RetryMustStartAtOne);
                }
                advertised_deadline
            }
        };

        self.pending = Some(PendingRenderApplication {
            connection_identity: update.connection_identity,
            identity: update.identity,
            claim: claim.token.clone(),
            max_attempts: update.retry_budget.max_attempts,
            attempt_ordinal: update.retry_budget.attempt_ordinal,
            deadline_millis,
        });
        Ok(())
    }

    pub fn settle(
        &mut self,
        coordinator: &mut DeliveryCoordinator,
        result: RenderApplicationResult,
        now_millis: u64,
    ) -> RenderApplicationSettleOutcome {
        if self.closed {
            return RenderApplicationSettleOutcome::TrackerClosed;
        }
        if let Some(expired) = self.expire_deadline(coordinator, now_millis) {
            return expired;
        }
        let Some(pending) = self.pending.as_ref() else {
            increment(&mut self.counters.stale_or_invalid_results);
            return RenderApplicationSettleOutcome::StaleOrDuplicate;
        };
        if let Err(error) =
            result.validate_for_identity(pending.identity, pending.connection_identity)
        {
            increment(&mut self.counters.stale_or_invalid_results);
            return RenderApplicationSettleOutcome::Rejected(error);
        }

        match result.outcome {
            RenderApplicationOutcome::Applied { .. } => {
                let pending = self
                    .pending
                    .take()
                    .expect("validated application remains present");
                let outcome = coordinator.acknowledge(&pending.claim);
                if outcome == CoordinatorSettleOutcome::Acknowledged {
                    self.retry = None;
                    increment(&mut self.counters.acknowledgements);
                    RenderApplicationSettleOutcome::Acknowledged
                } else {
                    self.retry = None;
                    self.closed = true;
                    close_same_instance_after_divergence(coordinator, outcome);
                    RenderApplicationSettleOutcome::CoordinatorDiverged(outcome)
                }
            }
            RenderApplicationOutcome::Nack(nack) => match nack.reason.recovery() {
                RenderApplicationNackRecovery::AuthoritativeResync => {
                    let pending = self
                        .pending
                        .take()
                        .expect("validated application remains present");
                    increment(&mut self.counters.resync_requests);
                    if pending.attempt_ordinal >= pending.max_attempts {
                        self.retry = None;
                        self.closed = true;
                        increment(&mut self.counters.retry_exhaustions);
                        RenderApplicationSettleOutcome::RetryExhausted {
                            reason: nack.reason,
                            coordinator: nack_terminal_fail_closed(coordinator, &pending.claim),
                        }
                    } else {
                        let retry = RenderRetryContext {
                            identity: RenderRetryIdentity::authoritative_resync(
                                pending.connection_identity,
                                pending.identity,
                            )
                            .expect("validated pending identity remains wire-representable"),
                            max_attempts: pending.max_attempts,
                            last_attempt_ordinal: pending.attempt_ordinal,
                            deadline_millis: pending.deadline_millis,
                        };
                        let outcome = coordinator.retry(&pending.claim);
                        if outcome == CoordinatorSettleOutcome::Retried {
                            self.retry = Some(retry);
                            increment(&mut self.counters.retries);
                            RenderApplicationSettleOutcome::AuthoritativeResyncScheduled {
                                reason: nack.reason,
                            }
                        } else {
                            self.retry = None;
                            self.closed = true;
                            close_same_instance_after_divergence(coordinator, outcome);
                            RenderApplicationSettleOutcome::CoordinatorDiverged(outcome)
                        }
                    }
                }
                RenderApplicationNackRecovery::BoundedRetry => {
                    let pending = self
                        .pending
                        .take()
                        .expect("validated application remains present");
                    if pending.attempt_ordinal >= pending.max_attempts {
                        self.retry = None;
                        self.closed = true;
                        increment(&mut self.counters.retry_exhaustions);
                        RenderApplicationSettleOutcome::RetryExhausted {
                            reason: nack.reason,
                            coordinator: nack_terminal_fail_closed(coordinator, &pending.claim),
                        }
                    } else {
                        let retry = RenderRetryContext {
                            identity: RenderRetryIdentity::from_identity(
                                pending.connection_identity,
                                pending.identity,
                            )
                            .expect("validated pending identity remains wire-representable"),
                            max_attempts: pending.max_attempts,
                            last_attempt_ordinal: pending.attempt_ordinal,
                            deadline_millis: pending.deadline_millis,
                        };
                        let outcome = coordinator.retry(&pending.claim);
                        if outcome == CoordinatorSettleOutcome::Retried {
                            self.retry = Some(retry);
                            increment(&mut self.counters.retries);
                            RenderApplicationSettleOutcome::Retried
                        } else {
                            self.retry = None;
                            self.closed = true;
                            close_same_instance_after_divergence(coordinator, outcome);
                            RenderApplicationSettleOutcome::CoordinatorDiverged(outcome)
                        }
                    }
                }
                RenderApplicationNackRecovery::Terminal => {
                    let pending = self
                        .pending
                        .take()
                        .expect("validated application remains present");
                    self.retry = None;
                    self.closed = true;
                    increment(&mut self.counters.terminal_nacks);
                    RenderApplicationSettleOutcome::FailedClosed {
                        reason: nack.reason,
                        coordinator: nack_terminal_fail_closed(coordinator, &pending.claim),
                    }
                }
            },
        }
    }

    /// Expire pending or ready-to-retry work without accepting a client result.
    pub fn expire_deadline(
        &mut self,
        coordinator: &mut DeliveryCoordinator,
        now_millis: u64,
    ) -> Option<RenderApplicationSettleOutcome> {
        if self.closed || now_millis < self.deadline_millis()? {
            return None;
        }
        let coordinator = match self.pending.take() {
            Some(pending) => nack_terminal_fail_closed(coordinator, &pending.claim),
            None => coordinator_close_as_settlement(coordinator.close()),
        };
        self.retry = None;
        self.closed = true;
        increment(&mut self.counters.deadline_expirations);
        Some(RenderApplicationSettleOutcome::DeadlineExpired {
            coordinator,
        })
    }

    /// Terminally close all admitted work on disconnect or shutdown.
    pub fn close(
        &mut self,
        coordinator: &mut DeliveryCoordinator,
    ) -> RenderApplicationCloseOutcome {
        if self.closed {
            return RenderApplicationCloseOutcome::AlreadyClosed;
        }
        self.pending = None;
        self.retry = None;
        self.closed = true;
        increment(&mut self.counters.closes);
        RenderApplicationCloseOutcome::Closed(coordinator.close())
    }
}

const fn coordinator_close_as_settlement(
    outcome: CoordinatorCloseOutcome,
) -> CoordinatorSettleOutcome {
    match outcome {
        CoordinatorCloseOutcome::Closed(_) => CoordinatorSettleOutcome::FailedClosed,
        CoordinatorCloseOutcome::AlreadyClosed => CoordinatorSettleOutcome::GenerationClosed,
    }
}

fn nack_terminal_fail_closed(
    coordinator: &mut DeliveryCoordinator,
    claim: &CoordinatorClaimToken,
) -> CoordinatorSettleOutcome {
    let outcome = coordinator.nack_terminal(claim);
    close_same_instance_after_divergence(coordinator, outcome);
    outcome
}

fn close_same_instance_after_divergence(
    coordinator: &mut DeliveryCoordinator,
    outcome: CoordinatorSettleOutcome,
) {
    if outcome == CoordinatorSettleOutcome::StaleOrDuplicate {
        let _ = coordinator.close();
    }
}

fn validate_render_claim_binding(
    claim: &CoordinatorDeliveryClaim,
    update: &RenderApplicationUpdate,
    retry_kind: Option<RenderApplicationKind>,
) -> Result<(), RenderApplicationBeginError> {
    let identity = update.identity;
    let token = identity.token;
    if token.coordinator_instance != claim.token.coordinator_instance().get() {
        return Err(RenderApplicationBeginError::ClaimMismatch(
            RenderApplicationClaimMismatch::CoordinatorInstance,
        ));
    }
    if update.connection_identity != claim.token.generation().connection_identity() {
        return Err(RenderApplicationBeginError::ClaimMismatch(
            RenderApplicationClaimMismatch::ConnectionIdentity,
        ));
    }
    if token.connection_generation != claim.token.generation().get() {
        return Err(RenderApplicationBeginError::ClaimMismatch(
            RenderApplicationClaimMismatch::ConnectionGeneration,
        ));
    }
    if token.scheduler_sequence != claim.token.obligation_sequence() {
        return Err(RenderApplicationBeginError::ClaimMismatch(
            RenderApplicationClaimMismatch::SchedulerSequence,
        ));
    }
    if token.attempt != claim.token.attempt() {
        return Err(RenderApplicationBeginError::ClaimMismatch(
            RenderApplicationClaimMismatch::Attempt,
        ));
    }

    let mut render = None;
    for effect in claim.plan.effects() {
        let candidate = match effect {
            PlannedEffect::RenderAuthority { claim, .. } => {
                Some((*claim, RenderApplicationKind::Delta))
            }
            PlannedEffect::RenderResync { authority, .. } => {
                Some((*authority, RenderApplicationKind::Snapshot))
            }
            _ => None,
        };
        if let Some(candidate) = candidate {
            if render.replace(candidate).is_some() {
                return Err(RenderApplicationBeginError::ClaimMismatch(
                    RenderApplicationClaimMismatch::MultipleRenderEffects,
                ));
            }
        }
    }
    let Some((authority, plan_kind)) = render else {
        return Err(RenderApplicationBeginError::ClaimMismatch(
            RenderApplicationClaimMismatch::MissingRenderEffect,
        ));
    };
    if authority.ledger_scope() == DeliveryScope::ResyncAll {
        return Err(RenderApplicationBeginError::ClaimMismatch(
            RenderApplicationClaimMismatch::GlobalResyncRequiresManifest,
        ));
    }
    let expected_kind = retry_kind.unwrap_or(plan_kind);
    if token.ledger_instance != authority.ledger_instance().get() {
        return Err(RenderApplicationBeginError::ClaimMismatch(
            RenderApplicationClaimMismatch::LedgerInstance,
        ));
    }
    if token.render_generation != authority.ledger_generation() {
        return Err(RenderApplicationBeginError::ClaimMismatch(
            RenderApplicationClaimMismatch::RenderGeneration,
        ));
    }
    if token.ledger_obligation != authority.ledger_obligation() {
        return Err(RenderApplicationBeginError::ClaimMismatch(
            RenderApplicationClaimMismatch::LedgerObligation,
        ));
    }
    if identity.kind != expected_kind {
        return Err(RenderApplicationBeginError::ClaimMismatch(
            RenderApplicationClaimMismatch::Kind,
        ));
    }
    match authority.ledger_scope() {
        DeliveryScope::Pane(pane_id) if pane_id != identity.pane_id => {
            return Err(RenderApplicationBeginError::ClaimMismatch(
                RenderApplicationClaimMismatch::Pane,
            ));
        }
        DeliveryScope::ResyncAll => unreachable!("global resync scope was rejected above"),
        DeliveryScope::Pane(_) => {}
    }
    if authority.source_version() != identity.resulting_state.state_sequence {
        return Err(RenderApplicationBeginError::ClaimMismatch(
            RenderApplicationClaimMismatch::SourceVersion,
        ));
    }
    Ok(())
}

#[derive(Clone, Debug)]
enum CoordinatorEntryState {
    Reserved {
        token: ReservationToken,
    },
    Ready {
        plan: DeliveryPlan,
    },
    InFlight {
        plan: DeliveryPlan,
        token: CoordinatorClaimToken,
    },
}

#[derive(Clone, Debug)]
struct CoordinatorEntry {
    sequence: u64,
    epoch: u64,
    footprint: PlanFootprint,
    state: CoordinatorEntryState,
    previous: Option<usize>,
    next: Option<usize>,
}

/// Hardened, non-wired authority coordinator.
///
/// V2 deliberately permits only one in-flight plan.  That conservative window
/// makes the causal proof exact: admission sequence, application sequence, and
/// acknowledgement sequence cannot diverge.  A later live implementation may
/// widen the window only with an equivalent ordered-commit proof.
pub struct DeliveryCoordinator {
    instance: CoordinatorInstance,
    generation: SchedulerGeneration,
    limits: HardenedSchedulerLimits,
    queue: Vec<Option<CoordinatorEntry>>,
    queue_free: Vec<usize>,
    queue_head: Option<usize>,
    queue_tail: Option<usize>,
    queue_len: usize,
    reservations: HashMap<u64, usize>,
    capacity: HardenedCapacity,
    next_sequence: Option<u64>,
    next_attempt: Option<u64>,
    current_epoch: u64,
    closed: bool,
    waiter: Option<Waker>,
    pending_wake: Option<Waker>,
    counters: HardenedCounters,
    last_close_report: Option<CoordinatorCloseReport>,
}

impl DeliveryCoordinator {
    pub fn try_new(
        connection_identity: RenderConnectionIdentity,
        generation_ordinal: u64,
        limits: HardenedSchedulerLimits,
    ) -> Result<Self, DeliveryCoordinatorCreateError> {
        let generation = mint_scheduler_generation(connection_identity, generation_ordinal)?;
        let instance = CoordinatorInstance::try_new()?;
        Ok(Self::from_authority(instance, generation, limits))
    }

    fn from_authority(
        instance: CoordinatorInstance,
        generation: SchedulerGeneration,
        limits: HardenedSchedulerLimits,
    ) -> Self {
        Self {
            instance,
            generation,
            limits,
            queue: Vec::new(),
            queue_free: Vec::new(),
            queue_head: None,
            queue_tail: None,
            queue_len: 0,
            reservations: HashMap::new(),
            capacity: HardenedCapacity::default(),
            next_sequence: Some(1),
            next_attempt: Some(1),
            current_epoch: 0,
            closed: false,
            waiter: None,
            pending_wake: None,
            counters: HardenedCounters::default(),
            last_close_report: None,
        }
    }

    #[cfg(test)]
    fn new(generation: SchedulerGeneration, limits: HardenedSchedulerLimits) -> Self {
        let instance = CoordinatorInstance::try_new()
            .expect("test delivery coordinator should have instance identity capacity");
        Self::from_authority(instance, generation, limits)
    }

    #[must_use]
    pub const fn instance_id(&self) -> CoordinatorInstanceId {
        self.instance.id()
    }

    #[must_use]
    pub const fn generation(&self) -> SchedulerGeneration {
        self.generation
    }

    #[must_use]
    pub const fn is_closed(&self) -> bool {
        self.closed
    }

    #[must_use]
    pub const fn counters(&self) -> HardenedCounters {
        self.counters
    }

    #[must_use]
    pub const fn last_close_report(&self) -> Option<CoordinatorCloseReport> {
        self.last_close_report
    }

    /// Return exact charged and semantic usage.
    ///
    /// Reserved plans charge slots and bytes but are not semantic effects.
    /// Ready and in-flight plans remain identically charged until ACK.
    #[must_use]
    pub const fn capacity(&self) -> HardenedCapacity {
        self.capacity
    }

    fn recompute_capacity(&self) -> HardenedCapacity {
        let mut capacity = HardenedCapacity::default();
        for entry in self.queue.iter().flatten() {
            for (used, entry_used) in capacity
                .lane_slots_used
                .iter_mut()
                .zip(entry.footprint.lane_slots)
            {
                *used += entry_used;
            }
            capacity.charged_slots += entry
                .footprint
                .total_slots()
                .expect("admitted footprint was checked");
            capacity.charged_resident_bytes += entry.footprint.resident_bytes;
            match &entry.state {
                CoordinatorEntryState::Reserved { .. } => {
                    capacity.reserved_plans += 1;
                }
                CoordinatorEntryState::Ready { .. } => {
                    capacity.ready_plans += 1;
                    capacity.semantic_effects += entry.footprint.semantic_effects;
                }
                CoordinatorEntryState::InFlight { .. } => {
                    capacity.in_flight_plans += 1;
                    capacity.semantic_effects += entry.footprint.semantic_effects;
                }
            }
        }
        capacity
    }

    fn queue_push_back(&mut self, mut entry: CoordinatorEntry) -> usize {
        let previous_tail = self.queue_tail;
        entry.previous = previous_tail;
        entry.next = None;
        let sequence = entry.sequence;
        let is_reservation = matches!(&entry.state, CoordinatorEntryState::Reserved { .. });
        let index = if let Some(index) = self.queue_free.pop() {
            self.queue[index] = Some(entry);
            index
        } else {
            let index = self.queue.len();
            self.queue.push(Some(entry));
            index
        };

        if let Some(tail) = previous_tail {
            self.queue[tail]
                .as_mut()
                .expect("coordinator tail must remain occupied")
                .next = Some(index);
        } else {
            debug_assert!(self.queue_head.is_none());
            self.queue_head = Some(index);
        }
        self.queue_tail = Some(index);
        self.queue_len = self
            .queue_len
            .checked_add(1)
            .expect("pending-plan limit bounds coordinator queue length");
        if is_reservation {
            let replaced = self.reservations.insert(sequence, index);
            debug_assert!(replaced.is_none());
        }
        index
    }

    fn queue_remove(&mut self, index: usize) -> CoordinatorEntry {
        let entry = self.queue[index]
            .as_ref()
            .expect("removed coordinator index must be occupied");
        let previous = entry.previous;
        let next = entry.next;

        if let Some(previous) = previous {
            self.queue[previous]
                .as_mut()
                .expect("coordinator predecessor must remain occupied")
                .next = next;
        } else {
            debug_assert_eq!(self.queue_head, Some(index));
            self.queue_head = next;
        }
        if let Some(next) = next {
            self.queue[next]
                .as_mut()
                .expect("coordinator successor must remain occupied")
                .previous = previous;
        } else {
            debug_assert_eq!(self.queue_tail, Some(index));
            self.queue_tail = previous;
        }

        let entry = self.queue[index]
            .take()
            .expect("unlinked coordinator entry must remain occupied");
        if matches!(&entry.state, CoordinatorEntryState::Reserved { .. }) {
            let removed = self.reservations.remove(&entry.sequence);
            debug_assert_eq!(removed, Some(index));
        }
        self.queue_free.push(index);
        self.queue_len = self
            .queue_len
            .checked_sub(1)
            .expect("occupied coordinator entry charges one queue slot");
        entry
    }

    fn queue_pop_front(&mut self) -> Option<CoordinatorEntry> {
        let index = self.queue_head?;
        Some(self.queue_remove(index))
    }

    fn queue_front(&self) -> Option<&CoordinatorEntry> {
        self.queue_head
            .and_then(|index| self.queue.get(index))
            .and_then(Option::as_ref)
    }

    fn queue_front_mut(&mut self) -> Option<&mut CoordinatorEntry> {
        let index = self.queue_head?;
        self.queue.get_mut(index).and_then(Option::as_mut)
    }

    fn charge_footprint(&mut self, footprint: PlanFootprint) {
        for (used, incoming) in self
            .capacity
            .lane_slots_used
            .iter_mut()
            .zip(footprint.lane_slots)
        {
            *used = used
                .checked_add(incoming)
                .expect("preflight proved lane capacity arithmetic");
        }
        self.capacity.charged_slots = self
            .capacity
            .charged_slots
            .checked_add(
                footprint
                    .total_slots()
                    .expect("validated footprint has representable slot total"),
            )
            .expect("preflight proved aggregate slot arithmetic");
        self.capacity.charged_resident_bytes = self
            .capacity
            .charged_resident_bytes
            .checked_add(footprint.resident_bytes)
            .expect("preflight proved resident-byte arithmetic");
    }

    fn release_footprint(&mut self, footprint: PlanFootprint) {
        for (used, released) in self
            .capacity
            .lane_slots_used
            .iter_mut()
            .zip(footprint.lane_slots)
        {
            *used = used
                .checked_sub(released)
                .expect("released lane charge was previously admitted");
        }
        self.capacity.charged_slots = self
            .capacity
            .charged_slots
            .checked_sub(
                footprint
                    .total_slots()
                    .expect("admitted footprint has representable slot total"),
            )
            .expect("released aggregate charge was previously admitted");
        self.capacity.charged_resident_bytes = self
            .capacity
            .charged_resident_bytes
            .checked_sub(footprint.resident_bytes)
            .expect("released resident-byte charge was previously admitted");
    }

    fn charge_ready_plan(&mut self, footprint: PlanFootprint) {
        self.charge_footprint(footprint);
        self.capacity.ready_plans = self
            .capacity
            .ready_plans
            .checked_add(1)
            .expect("pending-plan limit bounds ready count");
        self.capacity.semantic_effects = self
            .capacity
            .semantic_effects
            .checked_add(footprint.semantic_effects)
            .expect("slot limits bound semantic-effect count");
    }

    fn charge_reservation(&mut self, footprint: PlanFootprint) {
        self.charge_footprint(footprint);
        self.capacity.reserved_plans = self
            .capacity
            .reserved_plans
            .checked_add(1)
            .expect("pending-plan limit bounds reservation count");
    }

    fn commit_reserved_capacity(&mut self, footprint: PlanFootprint) {
        self.capacity.reserved_plans = self
            .capacity
            .reserved_plans
            .checked_sub(1)
            .expect("committed reservation was charged");
        self.capacity.ready_plans = self
            .capacity
            .ready_plans
            .checked_add(1)
            .expect("commit preserves the pending-plan count");
        self.capacity.semantic_effects = self
            .capacity
            .semantic_effects
            .checked_add(footprint.semantic_effects)
            .expect("slot limits bound semantic-effect count");
    }

    fn release_reservation(&mut self, footprint: PlanFootprint) {
        self.release_footprint(footprint);
        self.capacity.reserved_plans = self
            .capacity
            .reserved_plans
            .checked_sub(1)
            .expect("cancelled reservation was charged");
    }

    fn mark_ready_in_flight(&mut self) {
        self.capacity.ready_plans = self
            .capacity
            .ready_plans
            .checked_sub(1)
            .expect("claimed plan was ready");
        self.capacity.in_flight_plans = self
            .capacity
            .in_flight_plans
            .checked_add(1)
            .expect("single-consumer proof bounds in-flight count");
    }

    fn mark_in_flight_ready(&mut self) {
        self.capacity.in_flight_plans = self
            .capacity
            .in_flight_plans
            .checked_sub(1)
            .expect("retried plan was in flight");
        self.capacity.ready_plans = self
            .capacity
            .ready_plans
            .checked_add(1)
            .expect("retry preserves the pending-plan count");
    }

    fn release_in_flight(&mut self, footprint: PlanFootprint) {
        self.release_footprint(footprint);
        self.capacity.in_flight_plans = self
            .capacity
            .in_flight_plans
            .checked_sub(1)
            .expect("acknowledged plan was in flight");
        self.capacity.semantic_effects = self
            .capacity
            .semantic_effects
            .checked_sub(footprint.semantic_effects)
            .expect("acknowledged semantic effects were charged");
    }

    /// Atomically admit an already-authoritative multi-effect plan.
    ///
    /// Every effect must be a fixed-size durable authority handle.  If any lane
    /// or byte preflight fails, no queue, causal-clock, or capacity state is
    /// mutated and ownership of the complete plan is returned.
    pub fn admit_plan(&mut self, plan: DeliveryPlan) -> PlanAdmission {
        if self.closed {
            return PlanAdmission::Closed { plan };
        }
        let footprint = match self.validate_plan(&plan) {
            Ok(footprint) => footprint,
            Err(reason) => {
                increment(&mut self.counters.plans_rejected);
                return PlanAdmission::Rejected { reason, plan };
            }
        };
        if let Err(reason) = self.preflight(footprint) {
            increment(&mut self.counters.plans_rejected);
            return PlanAdmission::Rejected { reason, plan };
        }
        if footprint.barrier_count == 1 && self.current_epoch == u64::MAX {
            self.close_internal();
            return PlanAdmission::Closed { plan };
        }
        let Some(sequence) = self.issue_sequence() else {
            self.close_internal();
            return PlanAdmission::Closed { plan };
        };
        let epoch = self.current_epoch;
        self.queue_push_back(CoordinatorEntry {
            sequence,
            epoch,
            footprint,
            state: CoordinatorEntryState::Ready { plan },
            previous: None,
            next: None,
        });
        self.charge_ready_plan(footprint);
        if footprint.barrier_count == 1 {
            self.current_epoch += 1;
        }
        increment(&mut self.counters.plans_admitted);
        self.wake_waiter_if_claimable();
        PlanAdmission::Admitted { sequence, epoch }
    }

    /// Reserve all slots and resident bytes before an authoritative mutation.
    ///
    /// The reservation occupies its ingress sequence and is invisible to the
    /// consumer, but it is a causal fence: later plans cannot pass an
    /// uncommitted pre-mutation publication.
    pub fn reserve_plan(&mut self, plan: &DeliveryPlan) -> ReservationAdmission {
        if self.closed {
            return ReservationAdmission::Closed;
        }
        let footprint = match self.validate_plan(plan) {
            Ok(footprint) => footprint,
            Err(reason) => {
                increment(&mut self.counters.reservations_rejected);
                return ReservationAdmission::Rejected(reason);
            }
        };
        if let Err(reason) = self.preflight(footprint) {
            increment(&mut self.counters.reservations_rejected);
            return ReservationAdmission::Rejected(reason);
        }
        if footprint.barrier_count == 1 && self.current_epoch == u64::MAX {
            self.close_internal();
            return ReservationAdmission::Closed;
        }
        let Some(sequence) = self.issue_sequence() else {
            self.close_internal();
            return ReservationAdmission::Closed;
        };
        let token = ReservationToken {
            instance: self.instance.clone(),
            generation: self.generation,
            sequence,
        };
        let epoch = self.current_epoch;
        self.queue_push_back(CoordinatorEntry {
            sequence,
            epoch,
            footprint,
            state: CoordinatorEntryState::Reserved {
                token: token.clone(),
            },
            previous: None,
            next: None,
        });
        self.charge_reservation(footprint);
        if footprint.barrier_count == 1 {
            self.current_epoch += 1;
        }
        increment(&mut self.counters.reservations_admitted);
        ReservationAdmission::Reserved(token)
    }

    /// Publish a plan into an existing invisible reservation.
    ///
    /// Commit is atomic and capacity-infallible only when the plan has the exact
    /// reserved footprint.  A mismatch returns the complete plan and its
    /// instance-bound token while retaining the invisible reservation for
    /// explicit correction or cancellation.
    pub fn commit_reservation(
        &mut self,
        token: ReservationToken,
        plan: DeliveryPlan,
    ) -> ReservationCommit {
        if token.instance != self.instance {
            increment(&mut self.counters.wrong_instance_reservations);
            return ReservationCommit::WrongInstance { token, plan };
        }
        if self.closed {
            return ReservationCommit::Closed { plan };
        }
        if token.generation != self.generation {
            increment(&mut self.counters.stale_reservations);
            return ReservationCommit::Stale { plan };
        }
        let Some(index) = self.reservations.get(&token.sequence).copied() else {
            increment(&mut self.counters.stale_reservations);
            return ReservationCommit::Stale { plan };
        };
        let Some(reserved) = self.queue.get(index).and_then(Option::as_ref) else {
            increment(&mut self.counters.stale_reservations);
            return ReservationCommit::Stale { plan };
        };
        if !matches!(
            &reserved.state,
            CoordinatorEntryState::Reserved { token: queued } if queued == &token
        ) {
            increment(&mut self.counters.stale_reservations);
            return ReservationCommit::Stale { plan };
        }
        let footprint = match self.validate_plan(&plan) {
            Ok(footprint) => footprint,
            Err(reason) => {
                increment(&mut self.counters.reservation_commit_rejections);
                return ReservationCommit::Rejected {
                    token,
                    reason,
                    plan,
                };
            }
        };
        if reserved.footprint != footprint {
            increment(&mut self.counters.reservation_commit_rejections);
            return ReservationCommit::Rejected {
                token,
                reason: PlanRejectReason::FootprintMismatch,
                plan,
            };
        }
        let sequence = reserved.sequence;
        let epoch = reserved.epoch;
        self.queue[index]
            .as_mut()
            .expect("indexed reservation must remain occupied")
            .state = CoordinatorEntryState::Ready { plan };
        let removed = self.reservations.remove(&sequence);
        debug_assert_eq!(removed, Some(index));
        self.commit_reserved_capacity(footprint);
        increment(&mut self.counters.reservations_committed);
        self.wake_waiter_if_claimable();
        ReservationCommit::Committed { sequence, epoch }
    }

    /// Cancel an unpublished capacity reservation.
    ///
    /// A lifecycle reservation can roll its epoch back only while it is still
    /// the causal tail.  Once later work has observed the successor epoch, the
    /// reservation remains as a fail-closed fence until commit or generation
    /// shutdown.
    pub fn cancel_reservation(&mut self, token: ReservationToken) -> ReservationCancel {
        if token.instance != self.instance {
            increment(&mut self.counters.wrong_instance_reservations);
            return ReservationCancel::WrongInstance { token };
        }
        if self.closed {
            return ReservationCancel::GenerationClosed;
        }
        if token.generation != self.generation {
            increment(&mut self.counters.stale_reservations);
            return ReservationCancel::Stale;
        }
        let Some(index) = self.reservations.get(&token.sequence).copied() else {
            increment(&mut self.counters.stale_reservations);
            return ReservationCancel::Stale;
        };
        let Some(reserved) = self.queue.get(index).and_then(Option::as_ref) else {
            increment(&mut self.counters.stale_reservations);
            return ReservationCancel::Stale;
        };
        if !matches!(
            &reserved.state,
            CoordinatorEntryState::Reserved { token: queued } if queued == &token
        ) {
            increment(&mut self.counters.stale_reservations);
            return ReservationCancel::Stale;
        }
        if reserved.footprint.barrier_count == 1 && self.queue_tail != Some(index) {
            increment(&mut self.counters.reservation_cancel_rejections);
            return ReservationCancel::CausalDependents { token };
        }
        let rolled_back_epoch = (reserved.footprint.barrier_count == 1).then_some(reserved.epoch);
        let removed = self.queue_remove(index);
        self.release_reservation(removed.footprint);
        if let Some(epoch) = rolled_back_epoch {
            debug_assert_eq!(self.current_epoch, epoch + 1);
            self.current_epoch = epoch;
        }
        increment(&mut self.counters.reservations_cancelled);
        self.wake_waiter_if_claimable();
        ReservationCancel::Cancelled
    }

    /// Claim the oldest causally eligible atomic plan.
    ///
    /// The plan remains charged and retained in-flight until ACK, retry, or
    /// terminal NACK.  A reservation or earlier in-flight plan is an exact FIFO
    /// fence and yields no claim.
    pub fn claim_next(
        &mut self,
    ) -> Result<Option<CoordinatorDeliveryClaim>, CoordinatorClaimError> {
        if self.closed {
            return Ok(None);
        }
        let Some(front) = self.queue_front() else {
            return Ok(None);
        };
        let CoordinatorEntryState::Ready { .. } = &front.state else {
            return Ok(None);
        };
        let sequence = front.sequence;
        let epoch = front.epoch;
        let Some(attempt) = self.issue_attempt() else {
            self.close_internal();
            return Err(CoordinatorClaimError::TokenExhausted);
        };
        let token = CoordinatorClaimToken {
            instance: self.instance.clone(),
            generation: self.generation,
            obligation_sequence: sequence,
            attempt,
        };
        let front = self
            .queue_front_mut()
            .expect("ready front remains present during token issuance");
        let CoordinatorEntryState::Ready { plan } = &front.state else {
            unreachable!("ready front cannot change during exclusive claim")
        };
        let retained_plan = plan.clone();
        let prior_state = std::mem::replace(
            &mut front.state,
            CoordinatorEntryState::InFlight {
                plan: retained_plan,
                token: token.clone(),
            },
        );
        let CoordinatorEntryState::Ready { plan: claimed_plan } = prior_state else {
            unreachable!("exclusive claim replacement must recover the ready authority")
        };
        // A successful direct claim by the single logical consumer supersedes
        // a deferred wake that has not yet crossed the external owner-lock
        // boundary. Token exhaustion closes the generation and must preserve
        // the registered wake for extraction.
        self.waiter = None;
        self.pending_wake = None;
        self.mark_ready_in_flight();
        increment(&mut self.counters.claims);
        Ok(Some(CoordinatorDeliveryClaim {
            token,
            epoch,
            plan: claimed_plan,
        }))
    }

    /// Poll without a check-then-park race.
    ///
    /// Admission, settlement, and this poll must use the same exclusive owner.
    /// If no claim is eligible, registration occurs before that ownership can
    /// be released; the next eligibility transition defers it for
    /// [`Self::take_pending_wake`].
    pub fn poll_claim_next(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<CoordinatorDeliveryClaim>, CoordinatorClaimError>> {
        match self.claim_next() {
            Ok(None) if !self.closed => {
                self.waiter = Some(cx.waker().clone());
                increment(&mut self.counters.wake_registrations);
                Poll::Pending
            }
            result => Poll::Ready(result),
        }
    }

    /// Extract a registered consumer wake while retaining exclusive ownership.
    ///
    /// Every caller that performs a mutation which can make the causal head
    /// eligible or close the generation must call this before releasing its
    /// external owner guard. This includes claim/error paths that can close on
    /// token exhaustion. The returned wake must not be fired until after that
    /// guard is released. Extraction transfers the single pending wake
    /// obligation and accounts it as a delivery; dropping the returned value
    /// without waking violates the owner contract.
    pub fn take_pending_wake(&mut self) -> Option<PendingDeliveryWake> {
        let waiter = self.pending_wake.take()?;
        increment(&mut self.counters.wake_deliveries);
        Some(PendingDeliveryWake::new(waiter))
    }

    pub fn acknowledge(&mut self, token: &CoordinatorClaimToken) -> CoordinatorSettleOutcome {
        if token.instance != self.instance {
            increment(&mut self.counters.wrong_instance_settlements);
            return CoordinatorSettleOutcome::WrongInstance;
        }
        if self.closed {
            return CoordinatorSettleOutcome::GenerationClosed;
        }
        if !self.front_claim_matches(token) {
            increment(&mut self.counters.stale_settlements);
            return CoordinatorSettleOutcome::StaleOrDuplicate;
        }
        let Some(acknowledged) = self.queue_pop_front() else {
            unreachable!("matching claim remains present during exclusive acknowledgement")
        };
        self.release_in_flight(acknowledged.footprint);
        increment(&mut self.counters.acknowledgements);
        self.wake_waiter_if_claimable();
        CoordinatorSettleOutcome::Acknowledged
    }

    pub fn retry(&mut self, token: &CoordinatorClaimToken) -> CoordinatorSettleOutcome {
        if token.instance != self.instance {
            increment(&mut self.counters.wrong_instance_settlements);
            return CoordinatorSettleOutcome::WrongInstance;
        }
        if self.closed {
            return CoordinatorSettleOutcome::GenerationClosed;
        }
        if !self.front_claim_matches(token) {
            increment(&mut self.counters.stale_settlements);
            return CoordinatorSettleOutcome::StaleOrDuplicate;
        }
        let front = self
            .queue_front_mut()
            .expect("matching claim requires a front entry");
        let CoordinatorEntryState::InFlight { plan, .. } = &front.state else {
            unreachable!("matching claim requires in-flight state")
        };
        let plan = plan.clone();
        front.state = CoordinatorEntryState::Ready { plan };
        self.mark_in_flight_ready();
        increment(&mut self.counters.retries);
        self.wake_waiter_if_claimable();
        CoordinatorSettleOutcome::Retried
    }

    pub fn nack_terminal(&mut self, token: &CoordinatorClaimToken) -> CoordinatorSettleOutcome {
        if token.instance != self.instance {
            increment(&mut self.counters.wrong_instance_settlements);
            return CoordinatorSettleOutcome::WrongInstance;
        }
        if self.closed {
            return CoordinatorSettleOutcome::GenerationClosed;
        }
        if !self.front_claim_matches(token) {
            increment(&mut self.counters.stale_settlements);
            return CoordinatorSettleOutcome::StaleOrDuplicate;
        }
        increment(&mut self.counters.terminal_nacks);
        self.close_internal();
        CoordinatorSettleOutcome::FailedClosed
    }

    pub fn close(&mut self) -> CoordinatorCloseOutcome {
        if self.closed {
            return CoordinatorCloseOutcome::AlreadyClosed;
        }
        CoordinatorCloseOutcome::Closed(self.close_internal())
    }

    pub fn check_invariants(&self) -> Result<(), &'static str> {
        let capacity = self.capacity();
        if capacity != self.recompute_capacity() {
            return Err("incremental capacity accounting diverged from queue truth");
        }
        if self.queue_len > self.limits.pending_plans {
            return Err("pending plan count exceeds configured limit");
        }
        if self.queue_len + self.queue_free.len() != self.queue.len() {
            return Err("coordinator occupied and free slot counts disagree");
        }
        if (self.queue_len == 0) != (self.queue_head.is_none() && self.queue_tail.is_none()) {
            return Err("coordinator queue endpoints disagree with its length");
        }
        if capacity.charged_slots > self.limits.total_slots {
            return Err("charged slots exceed configured aggregate limit");
        }
        if capacity.charged_resident_bytes > self.limits.resident_bytes {
            return Err("charged resident bytes exceed configured limit");
        }
        for lane in 0..HARDENED_DELIVERY_LANE_COUNT {
            if capacity.lane_slots_used[lane] > self.limits.lane_slots[lane] {
                return Err("authority lane exceeds configured slot limit");
            }
        }

        let mut prior_sequence = None;
        let mut prior_epoch = None;
        let mut in_flight = 0usize;
        let mut semantic_effects = 0usize;
        let mut seen = HashSet::with_capacity(self.queue_len);
        let mut seen_reservations = HashSet::with_capacity(self.reservations.len());
        let mut cursor = self.queue_head;
        let mut previous = None;
        let mut position = 0usize;
        while let Some(index) = cursor {
            if position == self.queue_len || !seen.insert(index) {
                return Err("coordinator queue contains a cycle");
            }
            let entry = self
                .queue
                .get(index)
                .and_then(Option::as_ref)
                .ok_or("coordinator queue references a vacant or absent slot")?;
            if entry.previous != previous {
                return Err("coordinator predecessor link is inconsistent");
            }
            if prior_sequence.is_some_and(|prior| entry.sequence <= prior) {
                return Err("ingress sequences are not strictly increasing");
            }
            if prior_epoch.is_some_and(|prior| entry.epoch < prior) {
                return Err("causal epochs moved backwards");
            }
            prior_sequence = Some(entry.sequence);
            prior_epoch = Some(entry.epoch);
            match &entry.state {
                CoordinatorEntryState::Reserved { token } => {
                    if token.instance != self.instance
                        || token.generation != self.generation
                        || token.sequence != entry.sequence
                    {
                        return Err("reservation token does not identify its entry");
                    }
                    if self.reservations.get(&entry.sequence) != Some(&index)
                        || !seen_reservations.insert(entry.sequence)
                    {
                        return Err("reservation index does not identify its queue entry");
                    }
                }
                CoordinatorEntryState::Ready { plan } => {
                    semantic_effects = semantic_effects
                        .checked_add(entry.footprint.semantic_effects)
                        .ok_or("semantic effect accounting overflowed")?;
                    if self.validate_plan(plan).ok() != Some(entry.footprint) {
                        return Err("ready plan footprint or authority is invalid");
                    }
                }
                CoordinatorEntryState::InFlight { plan, token } => {
                    in_flight += 1;
                    if position != 0 {
                        return Err("only the causal head may be in flight");
                    }
                    semantic_effects = semantic_effects
                        .checked_add(entry.footprint.semantic_effects)
                        .ok_or("semantic effect accounting overflowed")?;
                    let token_obligation_sequence = token.obligation_sequence;
                    let entry_sequence = entry.sequence;
                    let token_identifies_entry = token.instance == self.instance
                        && token.generation == self.generation
                        && token_obligation_sequence == entry_sequence;
                    if !token_identifies_entry {
                        return Err("claim token does not identify its entry");
                    }
                    if self.validate_plan(plan).ok() != Some(entry.footprint) {
                        return Err("in-flight plan footprint or authority is invalid");
                    }
                }
            }
            previous = Some(index);
            cursor = entry.next;
            position += 1;
        }
        if position != self.queue_len || previous != self.queue_tail {
            return Err("coordinator queue traversal disagrees with its bounds");
        }
        if seen_reservations.len() != self.reservations.len() {
            return Err("reservation index retains an unordered entry");
        }
        let mut free = HashSet::with_capacity(self.queue_free.len());
        for index in &self.queue_free {
            if *index >= self.queue.len()
                || self.queue[*index].is_some()
                || !free.insert(*index)
            {
                return Err(
                    "coordinator free list references an occupied, absent, or duplicate slot",
                );
            }
        }
        for (index, slot) in self.queue.iter().enumerate() {
            if slot.is_some() != seen.contains(&index) || slot.is_none() != free.contains(&index) {
                return Err("coordinator slot is disconnected from queue and free-list authority");
            }
        }
        if in_flight > 1 || in_flight != capacity.in_flight_plans {
            return Err("in-flight accounting is inconsistent");
        }
        if capacity.semantic_effects != semantic_effects {
            return Err("semantic effect accounting is inconsistent");
        }
        if self.waiter.is_some()
            && self
                .queue_front()
                .is_some_and(|entry| matches!(&entry.state, CoordinatorEntryState::Ready { .. }))
        {
            return Err("registered waiter coexists with an eligible claim");
        }
        if self.waiter.is_some() && self.pending_wake.is_some() {
            return Err("registered waiter coexists with an undelivered pending wake");
        }
        if self.closed && (self.queue_len != 0 || self.waiter.is_some()) {
            return Err("closed generation retains scheduler-owned authority");
        }
        Ok(())
    }

    fn validate_plan(&self, plan: &DeliveryPlan) -> Result<PlanFootprint, PlanRejectReason> {
        if plan.effects.len() > self.limits.max_effects_per_plan {
            return Err(PlanRejectReason::TooManyEffects);
        }
        let footprint = plan.footprint().ok_or(PlanRejectReason::ResidentBytes)?;
        if footprint.barrier_count > 1 {
            return Err(PlanRejectReason::MultipleBarriers);
        }
        if footprint.barrier_count == 1 && footprint.semantic_effects != 1 {
            return Err(PlanRejectReason::BarrierMustBeIsolated);
        }
        for (index, effect) in plan.effects.iter().enumerate() {
            if effect.generation() != self.generation {
                return Err(PlanRejectReason::WrongGeneration);
            }
            if effect.retained_bytes() > self.limits.max_effect_resident_bytes {
                return Err(PlanRejectReason::EffectTooLarge);
            }
            match effect {
                PlannedEffect::RenderAuthority { key, claim, .. } => {
                    let DeliveryScope::Pane(pane_id) = claim.ledger_scope() else {
                        return Err(PlanRejectReason::RenderScopeMismatch);
                    };
                    if usize::try_from(key.scope()).ok() != Some(pane_id) {
                        return Err(PlanRejectReason::RenderScopeMismatch);
                    }
                }
                PlannedEffect::RenderResync { authority, .. }
                    if authority.ledger_scope() != DeliveryScope::ResyncAll =>
                {
                    return Err(PlanRejectReason::RenderScopeMismatch);
                }
                _ => {}
            }
            if let PlannedEffect::RenderAuthority {
                key: render_key,
                telemetry_through: Some(fence),
                ..
            } = effect
            {
                if fence.accumulator().generation() != self.generation {
                    return Err(PlanRejectReason::WrongGeneration);
                }
                let (has_matching_fold, has_ordered_matching_fold) =
                    plan.effects.iter().enumerate().fold(
                        (false, false),
                        |(has_matching, has_ordered), (fold_position, candidate)| match candidate {
                            PlannedEffect::TelemetryFold {
                                key,
                                accumulator,
                                through_version,
                                ..
                            } if *accumulator == fence.accumulator()
                                && *through_version >= fence.through_version() =>
                            {
                                (
                                    true,
                                    has_ordered || (key == render_key && fold_position < index),
                                )
                            }
                            _ => (has_matching, has_ordered),
                        },
                    );
                let dependency_is_ordered = if has_matching_fold {
                    has_ordered_matching_fold
                } else {
                    fence.accumulator().version() >= fence.through_version()
                };
                if !dependency_is_ordered {
                    return Err(PlanRejectReason::TelemetryDependencyOrder);
                }
            }
            if let PlannedEffect::TelemetryFold {
                accumulator,
                through_version,
                ..
            } = effect
                && accumulator.version() < *through_version
            {
                return Err(PlanRejectReason::TelemetryDependencyOrder);
            }
        }
        Ok(footprint)
    }

    fn preflight(&self, footprint: PlanFootprint) -> Result<(), PlanRejectReason> {
        let capacity = self.capacity;
        if self.queue_len >= self.limits.pending_plans {
            return Err(PlanRejectReason::Capacity);
        }
        let footprint_slots = footprint.total_slots().ok_or(PlanRejectReason::Capacity)?;
        if capacity
            .charged_slots
            .checked_add(footprint_slots)
            .is_none_or(|total| total > self.limits.total_slots)
        {
            return Err(PlanRejectReason::Capacity);
        }
        if capacity
            .charged_resident_bytes
            .checked_add(footprint.resident_bytes)
            .is_none_or(|total| total > self.limits.resident_bytes)
        {
            return Err(PlanRejectReason::ResidentBytes);
        }
        for lane in 0..HARDENED_DELIVERY_LANE_COUNT {
            if capacity.lane_slots_used[lane]
                .checked_add(footprint.lane_slots[lane])
                .is_none_or(|total| total > self.limits.lane_slots[lane])
            {
                return Err(PlanRejectReason::Capacity);
            }
        }
        Ok(())
    }

    fn front_claim_matches(&self, token: &CoordinatorClaimToken) -> bool {
        if token.generation != self.generation {
            return false;
        }
        self.queue_front().is_some_and(|front| {
            matches!(
                &front.state,
                CoordinatorEntryState::InFlight { token: current, .. } if current == token
            )
        })
    }

    fn issue_sequence(&mut self) -> Option<u64> {
        let sequence = self.next_sequence?;
        self.next_sequence = sequence.checked_add(1);
        Some(sequence)
    }

    fn issue_attempt(&mut self) -> Option<u64> {
        let attempt = self.next_attempt?;
        self.next_attempt = attempt.checked_add(1);
        Some(attempt)
    }

    fn close_internal(&mut self) -> CoordinatorCloseReport {
        if self.closed {
            return self.last_close_report.unwrap_or_default();
        }
        let capacity = self.capacity;
        let report = CoordinatorCloseReport {
            reserved_plans: capacity.reserved_plans,
            ready_plans: capacity.ready_plans,
            in_flight_plans: capacity.in_flight_plans,
            semantic_effects: capacity.semantic_effects,
            charged_slots: capacity.charged_slots,
            charged_resident_bytes: capacity.charged_resident_bytes,
        };
        self.queue.clear();
        self.queue_free.clear();
        self.queue_head = None;
        self.queue_tail = None;
        self.queue_len = 0;
        self.reservations.clear();
        self.capacity = HardenedCapacity::default();
        self.closed = true;
        self.last_close_report = Some(report);
        increment(&mut self.counters.closes);
        add_usize(
            &mut self.counters.closed_semantic_effects,
            report.semantic_effects,
        );
        add_usize(
            &mut self.counters.closed_charged_slots,
            report.charged_slots,
        );
        add_usize(
            &mut self.counters.closed_resident_bytes,
            report.charged_resident_bytes,
        );
        self.defer_waiter_wake();
        report
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

    fn wake_waiter_if_claimable(&mut self) {
        if self
            .queue_front()
            .is_some_and(|entry| matches!(&entry.state, CoordinatorEntryState::Ready { .. }))
        {
            self.defer_waiter_wake();
        }
    }

    #[cfg(test)]
    fn set_next_attempt_for_test(&mut self, next: Option<u64>) {
        self.next_attempt = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    type TestScheduler = DeliveryScheduler<u64, u64, u64, u64, u64, u64>;

    fn scheduler(limits: SchedulerLimits) -> TestScheduler {
        DeliveryScheduler::new(limits).expect("test capacities must be representable")
    }

    fn assert_accepted<T>(admission: Admission<T>) {
        assert!(matches!(
            admission.outcome(),
            AdmissionOutcome::Admitted | AdmissionOutcome::Coalesced | AdmissionOutcome::Escalated
        ));
        assert!(admission.returned_value().is_none());
    }

    #[test]
    fn zero_capacity_fifo_rejects_and_keyed_lanes_use_fixed_resync_slots() {
        let mut scheduler = scheduler(SchedulerLimits::new(0, 0, 0, 0));

        assert_eq!(scheduler.admit_lifecycle(1), AdmissionOutcome::Rejected);
        assert_eq!(scheduler.admit_payload(2), AdmissionOutcome::Rejected);
        assert_eq!(scheduler.admit_state(3, 30), AdmissionOutcome::Escalated);
        assert_eq!(scheduler.admit_render(4, 40), AdmissionOutcome::Escalated);
        assert_eq!(
            scheduler.capacity(),
            SchedulerCapacity {
                lifecycle: LaneCapacity { limit: 0, used: 0 },
                state_keys: LaneCapacity { limit: 0, used: 0 },
                state_resync: LaneCapacity { limit: 1, used: 1 },
                render_keys: LaneCapacity { limit: 0, used: 0 },
                render_resync: LaneCapacity { limit: 1, used: 1 },
                payload: LaneCapacity { limit: 0, used: 0 },
                durable_total: LaneCapacity { limit: 2, used: 2 },
                wake: LaneCapacity { limit: 1, used: 1 },
            }
        );
        assert_eq!(scheduler.pop_next(), Some(ScheduledItem::StateResync));
        assert_eq!(scheduler.pop_next(), Some(ScheduledItem::RenderResync));
        assert_eq!(scheduler.pop_next(), None);
        assert_eq!(scheduler.check_invariants(), Ok(()));
    }

    #[test]
    fn one_slot_fifo_lanes_are_strict_and_rejection_is_pre_admission() {
        let mut scheduler = scheduler(SchedulerLimits::new(1, 0, 0, 1));

        assert_eq!(scheduler.admit_lifecycle(10), AdmissionOutcome::Admitted);
        let before_reject = scheduler.capacity();
        let rejected_lifecycle = scheduler.admit_lifecycle(11);
        assert_eq!(rejected_lifecycle, AdmissionOutcome::Rejected);
        assert_eq!(rejected_lifecycle.into_returned(), Some(11));
        assert_eq!(scheduler.capacity(), before_reject);

        assert_eq!(scheduler.admit_payload(20), AdmissionOutcome::Admitted);
        let rejected_payload = scheduler.admit_payload(21);
        assert_eq!(rejected_payload, AdmissionOutcome::Rejected);
        assert_eq!(rejected_payload.into_returned(), Some(21));
        assert_eq!(scheduler.pop_next(), Some(ScheduledItem::Lifecycle(10)));
        assert_eq!(scheduler.pop_next(), Some(ScheduledItem::Payload(20)));
        assert_eq!(scheduler.pop_next(), None);
        assert_eq!(scheduler.counters().lifecycle.rejected, 1);
        assert_eq!(scheduler.counters().payload.rejected, 1);
    }

    #[test]
    fn fifo_lanes_preserve_exact_admission_order() {
        let mut scheduler = scheduler(SchedulerLimits::new(3, 0, 0, 3));
        for value in 1..=3 {
            assert_eq!(scheduler.admit_lifecycle(value), AdmissionOutcome::Admitted);
            assert_eq!(
                scheduler.admit_payload(value + 10),
                AdmissionOutcome::Admitted
            );
        }

        for expected in 1..=3 {
            assert_eq!(
                scheduler.pop_next(),
                Some(ScheduledItem::Lifecycle(expected))
            );
        }
        for expected in 11..=13 {
            assert_eq!(scheduler.pop_next(), Some(ScheduledItem::Payload(expected)));
        }
    }

    #[test]
    fn keyed_lanes_keep_latest_value_without_moving_round_robin_key() {
        let mut scheduler = scheduler(SchedulerLimits::new(0, 3, 3, 0));
        assert_eq!(scheduler.admit_state(1, 10), AdmissionOutcome::Admitted);
        assert_eq!(scheduler.admit_state(2, 20), AdmissionOutcome::Admitted);
        assert_eq!(scheduler.admit_state(1, 11), AdmissionOutcome::Coalesced);
        assert_eq!(scheduler.admit_render(7, 70), AdmissionOutcome::Admitted);
        assert_eq!(scheduler.admit_render(8, 80), AdmissionOutcome::Admitted);
        assert_eq!(scheduler.admit_render(7, 71), AdmissionOutcome::Coalesced);

        assert_eq!(
            scheduler.pop_next(),
            Some(ScheduledItem::State { key: 1, value: 11 })
        );
        assert_eq!(
            scheduler.pop_next(),
            Some(ScheduledItem::State { key: 2, value: 20 })
        );
        assert_eq!(
            scheduler.pop_next(),
            Some(ScheduledItem::Render { key: 7, value: 71 })
        );
        assert_eq!(
            scheduler.pop_next(),
            Some(ScheduledItem::Render { key: 8, value: 80 })
        );
    }

    #[test]
    fn keyed_overflow_is_one_authoritative_resync_that_supersedes_old_keys() {
        let mut scheduler = scheduler(SchedulerLimits::new(0, 1, 1, 0));
        assert_eq!(scheduler.admit_state(1, 10), AdmissionOutcome::Admitted);
        assert_eq!(scheduler.admit_state(2, 20), AdmissionOutcome::Escalated);
        assert_eq!(scheduler.admit_state(1, 11), AdmissionOutcome::Coalesced);
        assert_eq!(scheduler.capacity().state_keys.used, 1);
        assert_eq!(scheduler.capacity().state_resync.used, 1);

        assert_eq!(scheduler.admit_render(3, 30), AdmissionOutcome::Admitted);
        assert_eq!(scheduler.admit_render(4, 40), AdmissionOutcome::Escalated);
        assert_eq!(scheduler.pop_next(), Some(ScheduledItem::StateResync));
        assert_eq!(scheduler.capacity().state_keys.used, 0);
        assert_eq!(scheduler.capacity().state_resync.used, 0);
        assert_eq!(scheduler.pop_next(), Some(ScheduledItem::RenderResync));
        assert_eq!(scheduler.pop_next(), None);
        assert_eq!(scheduler.counters().state.escalated, 1);
        assert_eq!(scheduler.counters().render.escalated, 1);
    }

    #[test]
    fn large_keyed_resync_splices_and_reuses_bounded_stable_slots() {
        const KEYS: usize = 4096;
        let mut lane = KeyedLane::new(KEYS);
        for key in 0..KEYS {
            assert_eq!(lane.admit(key, key * 10), AdmissionOutcome::Admitted);
        }
        assert_eq!(lane.slots.len(), KEYS);
        assert_eq!(lane.active_len, KEYS);
        assert_eq!(lane.retired_len, 0);
        assert_eq!(lane.check_invariants(), Ok(()));

        assert_eq!(lane.admit(KEYS, KEYS * 10), AdmissionOutcome::Escalated);
        assert!(matches!(lane.pop(), Some(KeyedPop::Resync)));
        assert_eq!(lane.active_len, 0);
        assert_eq!(lane.retired_len, KEYS);
        assert_eq!(lane.slots.len(), KEYS);
        assert_eq!(lane.check_invariants(), Ok(()));

        for key in KEYS..KEYS * 2 {
            assert_eq!(lane.admit(key, key * 10), AdmissionOutcome::Admitted);
        }
        assert_eq!(lane.active_len, KEYS);
        assert_eq!(lane.retired_len, 0);
        assert_eq!(
            lane.slots.len(),
            KEYS,
            "post-resync admission must reuse retired storage without growth"
        );
        assert_eq!(lane.check_invariants(), Ok(()));

        for expected_key in KEYS..KEYS * 2 {
            match lane.pop() {
                Some(KeyedPop::Value { key, value }) => {
                    assert_eq!((key, value), (expected_key, expected_key * 10));
                }
                Some(KeyedPop::Resync) => panic!("unexpected second resync"),
                None => panic!("large keyed lane lost an admitted value"),
            }
        }
        assert!(lane.pop().is_none());
        assert_eq!(lane.slots.len(), KEYS);
        assert_eq!(lane.free.len(), KEYS);
        assert_eq!(lane.check_invariants(), Ok(()));
    }

    #[test]
    fn wake_is_a_coalesced_payload_free_hint_only() {
        let mut scheduler = scheduler(SchedulerLimits::new(1, 0, 0, 0));
        assert_eq!(scheduler.request_wake(), WakeOutcome::Admitted);
        assert_eq!(scheduler.request_wake(), WakeOutcome::Coalesced);
        assert!(scheduler.take_wake());
        assert!(!scheduler.take_wake());

        assert_eq!(scheduler.admit_lifecycle(1), AdmissionOutcome::Admitted);
        assert_eq!(scheduler.capacity().wake.used, 1);
        assert!(scheduler.take_wake());
        assert_eq!(
            scheduler.pop_next(),
            Some(ScheduledItem::Lifecycle(1)),
            "consuming a wake must not consume its durable obligation"
        );
        assert_eq!(
            scheduler.counters().wake,
            WakeCounters {
                admitted: 2,
                coalesced: 1,
                consumed: 2,
                ..WakeCounters::default()
            }
        );
    }

    #[test]
    fn bounded_priority_uses_the_imported_burst_limit() {
        let lane_capacity = DELIVERY_PRIORITY_BURST_LIMIT + 1;
        let mut scheduler = scheduler(SchedulerLimits::new(
            lane_capacity,
            lane_capacity,
            lane_capacity,
            lane_capacity,
        ));
        for value in 0..lane_capacity {
            assert_eq!(
                scheduler.admit_lifecycle(value as u64),
                AdmissionOutcome::Admitted
            );
            assert_eq!(
                scheduler.admit_state(value as u64, value as u64),
                AdmissionOutcome::Admitted
            );
            assert_eq!(
                scheduler.admit_render(value as u64, value as u64),
                AdmissionOutcome::Admitted
            );
            assert_eq!(
                scheduler.admit_payload(value as u64),
                AdmissionOutcome::Admitted
            );
        }

        let classes: Vec<_> = (0..=(DELIVERY_PRIORITY_BURST_LIMIT * 3))
            .map(|_| {
                scheduler
                    .pop_next()
                    .expect("all four lanes were populated")
                    .class()
            })
            .collect();
        assert!(
            classes[..DELIVERY_PRIORITY_BURST_LIMIT]
                .iter()
                .all(|class| *class == DurableClass::Lifecycle)
        );
        assert!(
            classes[DELIVERY_PRIORITY_BURST_LIMIT..DELIVERY_PRIORITY_BURST_LIMIT * 2]
                .iter()
                .all(|class| *class == DurableClass::State)
        );
        assert!(
            classes[DELIVERY_PRIORITY_BURST_LIMIT * 2..DELIVERY_PRIORITY_BURST_LIMIT * 3]
                .iter()
                .all(|class| *class == DurableClass::Render)
        );
        assert_eq!(
            classes[DELIVERY_PRIORITY_BURST_LIMIT * 3],
            DurableClass::Payload
        );
    }

    #[test]
    fn continuously_ready_mixed_classes_meet_the_executable_starvation_bound() {
        let mut scheduler = scheduler(SchedulerLimits::new(1, 1, 1, 1));
        assert_accepted(scheduler.admit_lifecycle(0));
        assert_accepted(scheduler.admit_state(0, 0));
        assert_accepted(scheduler.admit_render(0, 0));
        assert_accepted(scheduler.admit_payload(0));

        let mut classes = Vec::new();
        for next_value in 1..=256 {
            let item = scheduler
                .pop_next()
                .expect("every durable class is replenished after selection");
            let class = item.class();
            classes.push(class);
            match class {
                DurableClass::Lifecycle => {
                    assert_accepted(scheduler.admit_lifecycle(next_value));
                }
                DurableClass::State => {
                    assert_accepted(scheduler.admit_state(next_value, next_value));
                }
                DurableClass::Render => {
                    assert_accepted(scheduler.admit_render(next_value, next_value));
                }
                DurableClass::Payload => {
                    assert_accepted(scheduler.admit_payload(next_value));
                }
            }
        }

        for window in classes.windows(DELIVERY_MAX_STARVATION_DEQUEUES + 1) {
            let observed: HashSet<_> = window.iter().copied().collect();
            assert_eq!(
                observed.len(),
                DELIVERY_DURABLE_CLASS_COUNT,
                "every continuously ready class must appear within the starvation bound"
            );
        }
    }

    #[test]
    fn shutdown_is_terminal_and_accounts_for_every_discarded_obligation() {
        let mut scheduler = scheduler(SchedulerLimits::new(2, 1, 1, 2));
        assert_accepted(scheduler.admit_lifecycle(1));
        assert_accepted(scheduler.admit_lifecycle(2));
        assert_accepted(scheduler.admit_state(1, 1));
        assert_accepted(scheduler.admit_state(2, 2));
        assert_accepted(scheduler.admit_render(1, 1));
        assert_accepted(scheduler.admit_payload(1));
        assert_eq!(scheduler.capacity().durable_total.used, 6);

        assert_eq!(
            scheduler.close(),
            SchedulerCloseOutcome::Closed {
                discarded_obligations: 6
            }
        );
        assert_eq!(scheduler.close(), SchedulerCloseOutcome::AlreadyClosed);
        assert!(scheduler.is_closed());
        assert_eq!(scheduler.capacity().durable_total.used, 0);
        assert_eq!(scheduler.capacity().wake.used, 0);
        assert_eq!(scheduler.pop_next(), None);
        let closed_lifecycle = scheduler.admit_lifecycle(3);
        assert_eq!(closed_lifecycle, AdmissionOutcome::Closed);
        assert_eq!(closed_lifecycle.into_returned(), Some(3));
        let closed_state = scheduler.admit_state(3, 3);
        assert_eq!(closed_state, AdmissionOutcome::Closed);
        assert_eq!(closed_state.into_returned(), Some((3, 3)));
        let closed_render = scheduler.admit_render(3, 3);
        assert_eq!(closed_render, AdmissionOutcome::Closed);
        assert_eq!(closed_render.into_returned(), Some((3, 3)));
        let closed_payload = scheduler.admit_payload(3);
        assert_eq!(closed_payload, AdmissionOutcome::Closed);
        assert_eq!(closed_payload.into_returned(), Some(3));
        assert_eq!(scheduler.request_wake(), WakeOutcome::Closed);
        assert_eq!(scheduler.counters().shutdowns, 1);
        assert_eq!(scheduler.counters().discarded_on_shutdown, 6);
        assert_eq!(scheduler.check_invariants(), Ok(()));
    }

    #[test]
    fn counters_exactly_classify_every_outcome_and_dequeue() {
        let mut scheduler = scheduler(SchedulerLimits::new(1, 1, 1, 1));

        assert_eq!(scheduler.admit_lifecycle(1), AdmissionOutcome::Admitted);
        assert!(scheduler.take_wake());
        assert_eq!(scheduler.admit_lifecycle(2), AdmissionOutcome::Rejected);
        assert_eq!(scheduler.pop_next(), Some(ScheduledItem::Lifecycle(1)));

        assert_eq!(scheduler.admit_state(1, 10), AdmissionOutcome::Admitted);
        assert!(scheduler.take_wake());
        assert_eq!(scheduler.admit_state(1, 11), AdmissionOutcome::Coalesced);
        assert!(scheduler.take_wake());
        assert_eq!(scheduler.admit_state(2, 20), AdmissionOutcome::Escalated);
        assert!(scheduler.take_wake());
        assert_eq!(scheduler.pop_next(), Some(ScheduledItem::StateResync));

        assert_eq!(scheduler.admit_render(1, 10), AdmissionOutcome::Admitted);
        assert!(scheduler.take_wake());
        assert_eq!(scheduler.admit_payload(1), AdmissionOutcome::Admitted);
        assert!(scheduler.take_wake());
        assert_eq!(scheduler.admit_payload(2), AdmissionOutcome::Rejected);
        assert_eq!(
            scheduler.pop_next(),
            Some(ScheduledItem::Render { key: 1, value: 10 })
        );
        assert_eq!(scheduler.pop_next(), Some(ScheduledItem::Payload(1)));

        assert_eq!(
            scheduler.counters(),
            SchedulerCounters {
                lifecycle: ClassCounters {
                    admitted: 1,
                    rejected: 1,
                    dequeued: 1,
                    ..ClassCounters::default()
                },
                state: ClassCounters {
                    admitted: 1,
                    coalesced: 1,
                    escalated: 1,
                    dequeued: 1,
                    ..ClassCounters::default()
                },
                render: ClassCounters {
                    admitted: 1,
                    dequeued: 1,
                    ..ClassCounters::default()
                },
                payload: ClassCounters {
                    admitted: 1,
                    rejected: 1,
                    dequeued: 1,
                    ..ClassCounters::default()
                },
                wake: WakeCounters {
                    admitted: 6,
                    consumed: 6,
                    ..WakeCounters::default()
                },
                ..SchedulerCounters::default()
            }
        );

        assert_eq!(
            scheduler.close(),
            SchedulerCloseOutcome::Closed {
                discarded_obligations: 0
            }
        );
        assert_eq!(scheduler.admit_lifecycle(3), AdmissionOutcome::Closed);
        assert_eq!(scheduler.admit_state(3, 3), AdmissionOutcome::Closed);
        assert_eq!(scheduler.admit_render(3, 3), AdmissionOutcome::Closed);
        assert_eq!(scheduler.admit_payload(3), AdmissionOutcome::Closed);
        assert_eq!(scheduler.request_wake(), WakeOutcome::Closed);
        let counters = scheduler.counters();
        assert_eq!(counters.lifecycle.closed, 1);
        assert_eq!(counters.state.closed, 1);
        assert_eq!(counters.render.closed, 1);
        assert_eq!(counters.payload.closed, 1);
        assert_eq!(counters.wake.closed, 1);
        assert_eq!(counters.shutdowns, 1);
    }

    #[test]
    fn aggregate_capacity_overflow_is_rejected_at_construction() {
        assert!(matches!(
            TestScheduler::new(SchedulerLimits::new(usize::MAX, 0, 0, 0)),
            Err(SchedulerConfigError::CapacityOverflow)
        ));
    }

    proptest! {
        #[test]
        fn fifo_pre_admission_never_mutates_on_rejection(
            capacity in 0usize..=16,
            values in prop::collection::vec(any::<u64>(), 0..64),
        ) {
            let mut scheduler = scheduler(SchedulerLimits::new(capacity, 0, 0, 0));
            let admitted: Vec<_> = values.iter().copied().take(capacity).collect();

            for (index, value) in values.into_iter().enumerate() {
                let before = scheduler.capacity().lifecycle.used;
                let outcome = scheduler.admit_lifecycle(value);
                if index < capacity {
                    prop_assert_eq!(outcome, AdmissionOutcome::Admitted);
                    prop_assert_eq!(scheduler.capacity().lifecycle.used, before + 1);
                } else {
                    prop_assert_eq!(outcome, AdmissionOutcome::Rejected);
                    prop_assert_eq!(scheduler.capacity().lifecycle.used, before);
                }
                prop_assert_eq!(scheduler.check_invariants(), Ok(()));
            }

            let mut drained = Vec::new();
            while let Some(ScheduledItem::Lifecycle(value)) = scheduler.pop_next() {
                drained.push(value);
            }
            prop_assert_eq!(drained, admitted);
        }

        #[test]
        fn arbitrary_operations_preserve_bounds_and_closed_semantics(
            limits in (0usize..=8, 0usize..=8, 0usize..=8, 0usize..=8),
            actions in prop::collection::vec((0u8..=8, any::<u16>(), any::<u16>()), 0..256),
        ) {
            let mut scheduler = scheduler(SchedulerLimits::new(
                limits.0,
                limits.1,
                limits.2,
                limits.3,
            ));

            for (action, key, value) in actions {
                match action {
                    0 => {
                        let _ = scheduler.admit_lifecycle(u64::from(value));
                    }
                    1 => {
                        let _ = scheduler.admit_state(u64::from(key), u64::from(value));
                    }
                    2 => {
                        let _ = scheduler.admit_render(u64::from(key), u64::from(value));
                    }
                    3 => {
                        let _ = scheduler.admit_payload(u64::from(value));
                    }
                    4 => {
                        let _ = scheduler.request_wake();
                    }
                    5 => {
                        let _ = scheduler.take_wake();
                    }
                    6 => {
                        let _ = scheduler.pop_next();
                    }
                    7 => {
                        let _ = scheduler.close();
                    }
                    8 => {
                        let _ = scheduler.capacity();
                    }
                    _ => unreachable!("generated action is constrained to 0..=8"),
                }

                let capacity = scheduler.capacity();
                prop_assert!(capacity.lifecycle.used <= capacity.lifecycle.limit);
                prop_assert!(capacity.state_keys.used <= capacity.state_keys.limit);
                prop_assert!(capacity.state_resync.used <= 1);
                prop_assert!(capacity.render_keys.used <= capacity.render_keys.limit);
                prop_assert!(capacity.render_resync.used <= 1);
                prop_assert!(capacity.payload.used <= capacity.payload.limit);
                prop_assert!(capacity.durable_total.used <= capacity.durable_total.limit);
                prop_assert!(capacity.wake.used <= 1);
                prop_assert_eq!(scheduler.check_invariants(), Ok(()));
            }
        }
    }
}

#[cfg(test)]
mod hardened_tests {
    use super::*;
    use proptest::prelude::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Wake;

    const CONNECTION_IDENTITY: RenderConnectionIdentity = RenderConnectionIdentity::new(
        codec::TopologyStreamId::from_bytes([0x41; 16]),
        mux::MuxSessionIncarnation::from_bytes([0x43; 16]),
    );
    const GENERATION: SchedulerGeneration =
        SchedulerGeneration::for_test(CONNECTION_IDENTITY, 41, 1);

    fn broad_limits() -> HardenedSchedulerLimits {
        HardenedSchedulerLimits::new([16; HARDENED_DELIVERY_LANE_COUNT], 64, 4096, 8, 1024, 64)
            .expect("broad test limits are representable")
    }

    fn coordinator() -> DeliveryCoordinator {
        DeliveryCoordinator::new(GENERATION, broad_limits())
    }

    fn handle(id: u64, version: u64) -> AuthorityHandle {
        AuthorityHandle::new(GENERATION, id, version)
    }

    fn plan_resident_bytes(effect_count: usize, additional_retained_bytes: usize) -> usize {
        std::mem::size_of::<CoordinatorEntry>()
            + 2 * std::mem::size_of::<usize>()
            + std::mem::size_of::<PlannedEffect>() * effect_count
            + additional_retained_bytes
    }

    fn event_plan(id: u64, retained_bytes: usize) -> DeliveryPlan {
        DeliveryPlan::new(vec![PlannedEffect::EventJournal {
            journal: handle(id, 1),
            retained_bytes,
        }])
        .expect("single event is a valid plan")
    }

    fn state_plan(id: u64) -> DeliveryPlan {
        DeliveryPlan::new(vec![PlannedEffect::StateAuthority {
            key: StableEffectKey::new(id, 1),
            authority: handle(1000 + id, 1),
            retained_bytes: 1,
        }])
        .expect("single state authority is a valid plan")
    }

    fn barrier_plan(id: u64) -> DeliveryPlan {
        DeliveryPlan::new(vec![PlannedEffect::LifecycleBarrier {
            scope: BarrierScope::Global,
            journal: handle(2000 + id, 1),
            retained_bytes: 1,
        }])
        .expect("single barrier is a valid plan")
    }

    fn plan_for_lane(lane: AuthorityLane, id: u64) -> DeliveryPlan {
        match lane {
            AuthorityLane::Event => event_plan(id, 1),
            AuthorityLane::Lifecycle => barrier_plan(id),
            _ => panic!("test oracle only models event and lifecycle lanes"),
        }
    }

    fn wrong_generation_plan(lane: AuthorityLane, id: u64) -> DeliveryPlan {
        let wrong_generation = SchedulerGeneration::for_test(
            CONNECTION_IDENTITY,
            GENERATION.get() + 1,
            GENERATION.instance_id().get() + 1,
        );
        let effect = match lane {
            AuthorityLane::Event => PlannedEffect::EventJournal {
                journal: AuthorityHandle::new(wrong_generation, id, 1),
                retained_bytes: 1,
            },
            AuthorityLane::Lifecycle => PlannedEffect::LifecycleBarrier {
                scope: BarrierScope::Global,
                journal: AuthorityHandle::new(wrong_generation, id, 1),
                retained_bytes: 1,
            },
            _ => panic!("test oracle only models event and lifecycle lanes"),
        };
        DeliveryPlan::new(vec![effect]).expect("wrong generation does not invalidate plan shape")
    }

    fn render_plan(id: u64) -> DeliveryPlan {
        DeliveryPlan::new(vec![PlannedEffect::RenderAuthority {
            key: StableEffectKey::new(id, 1),
            claim: ExternalRenderAuthority::new_for_test(
                GENERATION,
                1,
                7,
                id,
                DeliveryScope::Pane(usize::try_from(id).expect("test pane ID should fit usize")),
                1,
            ),
            telemetry_through: None,
            retained_bytes: 1,
        }])
        .expect("external render claim is a valid plan")
    }

    fn render_application_update(
        claim: &CoordinatorDeliveryClaim,
        attempt_ordinal: u16,
        max_attempts: u16,
        remaining_millis: u32,
    ) -> RenderApplicationUpdate {
        let (authority, kind) = match claim.plan().effects() {
            [PlannedEffect::RenderAuthority { claim, .. }] => {
                (*claim, RenderApplicationKind::Delta)
            }
            [PlannedEffect::RenderResync { authority, .. }] => {
                (*authority, RenderApplicationKind::Snapshot)
            }
            _ => panic!("test render application requires exactly one render effect"),
        };
        let pane_id = match authority.ledger_scope() {
            DeliveryScope::Pane(pane_id) => pane_id,
            DeliveryScope::ResyncAll => 9,
        };
        let resulting_state = codec::RenderStateIdentity {
            render_generation: authority.ledger_generation(),
            state_sequence: authority.source_version(),
        };
        let base_state = (kind == RenderApplicationKind::Delta).then_some(
            codec::RenderStateIdentity {
                render_generation: authority.ledger_generation(),
                state_sequence: authority.source_version().saturating_sub(1),
            },
        );
        let surface_sequence = usize::try_from(resulting_state.state_sequence)
            .expect("test render sequence should fit usize");
        let bonus_lines = if kind == RenderApplicationKind::Snapshot {
            codec::SerializedLines::from(
                (0isize..24)
                    .map(|row| (row, wezterm_term::Line::with_width(80, surface_sequence)))
                    .collect::<Vec<_>>(),
            )
        } else {
            codec::SerializedLines::default()
        };
        RenderApplicationUpdate {
            identity: RenderApplicationIdentity {
                protocol_version: codec::RENDER_APPLICATION_PROTOCOL_VERSION,
                token: codec::RenderApplicationToken {
                    connection_generation: claim.token().generation().get(),
                    coordinator_instance: claim.token().coordinator_instance().get(),
                    scheduler_sequence: claim.token().obligation_sequence(),
                    attempt: claim.token().attempt(),
                    ledger_instance: authority.ledger_instance().get(),
                    render_generation: authority.ledger_generation(),
                    ledger_obligation: authority.ledger_obligation(),
                },
                pane_id,
                base_state,
                resulting_state,
                kind,
            },
            retry_budget: codec::RenderApplicationRetryBudget {
                attempt_ordinal,
                max_attempts,
                remaining_millis,
            },
            surface: codec::GetPaneRenderChangesResponse {
                pane_id,
                mouse_grabbed: false,
                alt_screen_active: false,
                cursor_position: mux::renderable::StableCursorPosition::default(),
                dimensions: mux::renderable::RenderableDimensions {
                    cols: 80,
                    viewport_rows: 24,
                    scrollback_rows: 24,
                    physical_top: 0,
                    scrollback_top: 0,
                    dpi: 96,
                    pixel_width: 800,
                    pixel_height: 480,
                    reverse_video: false,
                },
                tiered_scrollback_status: None,
                dirty_lines: Vec::new(),
                title: "render-application".to_string(),
                working_dir: None,
                bonus_lines,
                input_serial: None,
                seqno: surface_sequence,
            },
            semantic_zones: if kind == RenderApplicationKind::Snapshot {
                codec::RenderComponentUpdate::Replace(codec::GetSemanticZonesResponse {
                    pane_id,
                    zones: Vec::new(),
                    zone_texts: Vec::new(),
                    last_exit_code: None,
                })
            } else {
                codec::RenderComponentUpdate::Unchanged
            },
            palette: if kind == RenderApplicationKind::Snapshot {
                codec::RenderComponentUpdate::Replace(codec::SetPalette {
                    pane_id,
                    palette: wezterm_term::color::ColorPalette::default(),
                })
            } else {
                codec::RenderComponentUpdate::Unchanged
            },
            alerts: Vec::new(),
            connection_identity: claim.token().generation().connection_identity(),
        }
    }

    fn applied_result(update: &RenderApplicationUpdate) -> RenderApplicationResult {
        RenderApplicationResult {
            identity: update.identity,
            outcome: RenderApplicationOutcome::Applied {
                applied_state: update.identity.resulting_state,
            },
            connection_identity: update.connection_identity,
        }
    }

    fn nack_result(
        update: &RenderApplicationUpdate,
        reason: RenderApplicationNackReason,
    ) -> RenderApplicationResult {
        let observed_state = if matches!(
            reason,
            RenderApplicationNackReason::BaseMismatch
                | RenderApplicationNackReason::GenerationMismatch
                | RenderApplicationNackReason::DetectedGap
        ) {
            codec::RenderApplicationObservedState::Applied(codec::RenderStateIdentity {
                render_generation: update.identity.resulting_state.render_generation,
                state_sequence: update
                    .identity
                    .resulting_state
                    .state_sequence
                    .saturating_sub(1),
            })
        } else {
            codec::RenderApplicationObservedState::NotApplicable
        };
        RenderApplicationResult {
            identity: update.identity,
            outcome: RenderApplicationOutcome::Nack(codec::RenderApplicationNack {
                reason,
                observed_state,
            }),
            connection_identity: update.connection_identity,
        }
    }

    fn claimed_render(
        coordinator: &mut DeliveryCoordinator,
        pane_id: u64,
    ) -> CoordinatorDeliveryClaim {
        assert!(matches!(
            coordinator.admit_plan(render_plan(pane_id)),
            PlanAdmission::Admitted { .. }
        ));
        coordinator
            .claim_next()
            .expect("test claim identity space remains available")
            .expect("admitted render plan is claimable")
    }

    #[test]
    fn coordinator_instance_allocation_fails_before_wrap_or_reuse() {
        let counter = AtomicU64::new(u64::MAX - 1);
        assert_eq!(
            allocate_coordinator_instance(&counter),
            Ok(CoordinatorInstanceId(u64::MAX - 1))
        );
        assert_eq!(
            allocate_coordinator_instance(&counter),
            Err(CoordinatorInstanceExhausted)
        );
        assert_eq!(
            allocate_coordinator_instance(&counter),
            Err(CoordinatorInstanceExhausted)
        );
    }

    #[test]
    fn scheduler_generation_allocation_fails_before_wrap_or_reuse() {
        let counter = AtomicU64::new(u64::MAX - 1);
        assert_eq!(
            allocate_scheduler_generation_instance(&counter),
            Ok(SchedulerGenerationInstanceId(u64::MAX - 1))
        );
        assert_eq!(
            allocate_scheduler_generation_instance(&counter),
            Err(SchedulerGenerationError::InstanceExhausted)
        );
        assert_eq!(
            allocate_scheduler_generation_instance(&counter),
            Err(SchedulerGenerationError::InstanceExhausted)
        );
    }

    #[test]
    fn production_generation_mint_rejects_aliasing_and_reserved_authority() {
        let reserved = RenderConnectionIdentity::new(
            codec::TopologyStreamId::from_bytes([0; 16]),
            mux::MuxSessionIncarnation::from_bytes([0; 16]),
        );
        assert!(matches!(
            DeliveryCoordinator::try_new(reserved, 41, broad_limits()),
            Err(DeliveryCoordinatorCreateError::SchedulerGeneration(
                SchedulerGenerationError::InvalidConnectionIdentity
            ))
        ));
        assert!(matches!(
            DeliveryCoordinator::try_new(CONNECTION_IDENTITY, 0, broad_limits()),
            Err(DeliveryCoordinatorCreateError::SchedulerGeneration(
                SchedulerGenerationError::ZeroOrdinal
            ))
        ));

        let origin = DeliveryCoordinator::try_new(CONNECTION_IDENTITY, 41, broad_limits())
            .expect("first production scheduler generation should mint");
        let mut replacement =
            DeliveryCoordinator::try_new(CONNECTION_IDENTITY, 41, broad_limits())
                .expect("same numeric ordinal should mint a distinct scheduler instance");
        assert_eq!(origin.generation().get(), replacement.generation().get());
        assert_eq!(
            origin.generation().connection_identity(),
            replacement.generation().connection_identity()
        );
        assert_ne!(
            origin.generation().instance_id(),
            replacement.generation().instance_id()
        );

        let old_plan = DeliveryPlan::new(vec![PlannedEffect::EventJournal {
            journal: AuthorityHandle::new(origin.generation(), 9, 1),
            retained_bytes: 1,
        }])
        .expect("old scheduler authority is a valid plan shape");
        assert!(matches!(
            replacement.admit_plan(old_plan),
            PlanAdmission::Rejected {
                reason: PlanRejectReason::WrongGeneration,
                ..
            }
        ));
    }

    #[test]
    fn application_ack_is_the_only_path_that_releases_the_exact_claim() {
        let mut coordinator = coordinator();
        let claim = claimed_render(&mut coordinator, 9);
        let update = render_application_update(&claim, 1, 3, 100);
        let mut tracker = RenderApplicationSettlementTracker::new();
        tracker
            .begin(&claim, &update, 1_000)
            .expect("exact render claim should bind");

        assert_eq!(coordinator.capacity().in_flight_plans, 1);
        assert_eq!(
            tracker.settle(&mut coordinator, applied_result(&update), 1_050),
            RenderApplicationSettleOutcome::Acknowledged
        );
        assert_eq!(coordinator.capacity(), HardenedCapacity::default());
        assert_eq!(
            tracker.counters(),
            RenderApplicationSettlementCounters {
                applications_begun: 1,
                acknowledgements: 1,
                ..RenderApplicationSettlementCounters::default()
            }
        );
    }

    #[test]
    fn application_begin_rejects_foreign_connection_with_same_numeric_generation() {
        let mut coordinator = coordinator();
        let claim = claimed_render(&mut coordinator, 9);
        let mut update = render_application_update(&claim, 1, 3, 100);
        update.connection_identity = RenderConnectionIdentity::new(
            codec::TopologyStreamId::from_bytes([0x71; 16]),
            mux::MuxSessionIncarnation::from_bytes([0x73; 16]),
        );
        let mut tracker = RenderApplicationSettlementTracker::new();
        assert_eq!(
            tracker.begin(&claim, &update, 1_000),
            Err(RenderApplicationBeginError::ClaimMismatch(
                RenderApplicationClaimMismatch::ConnectionIdentity
            ))
        );
        assert_eq!(coordinator.capacity().in_flight_plans, 1);
        assert_eq!(tracker.pending_identity(), None);
    }

    #[test]
    fn stale_or_wrong_attempt_ack_cannot_commit_live_work() {
        let mut coordinator = coordinator();
        let claim = claimed_render(&mut coordinator, 9);
        let update = render_application_update(&claim, 1, 3, 100);
        let mut tracker = RenderApplicationSettlementTracker::new();
        tracker
            .begin(&claim, &update, 1_000)
            .expect("exact render claim should bind");

        let mut stale = applied_result(&update);
        stale.identity.token.attempt += 1;
        assert_eq!(
            tracker.settle(&mut coordinator, stale, 1_010),
            RenderApplicationSettleOutcome::Rejected(
                RenderApplicationContractError::SettlementIdentityMismatch
            )
        );
        assert_eq!(coordinator.capacity().in_flight_plans, 1);
        assert_eq!(
            tracker.settle(&mut coordinator, applied_result(&update), 1_020),
            RenderApplicationSettleOutcome::Acknowledged
        );
    }

    #[test]
    fn same_instance_coordinator_divergence_closes_the_generation() {
        let mut coordinator = coordinator();
        let claim = claimed_render(&mut coordinator, 9);
        let update = render_application_update(&claim, 1, 3, 100);
        let mut tracker = RenderApplicationSettlementTracker::new();
        tracker
            .begin(&claim, &update, 1_000)
            .expect("exact render claim should bind");
        assert_eq!(
            coordinator.acknowledge(claim.token()),
            CoordinatorSettleOutcome::Acknowledged
        );
        assert!(!coordinator.is_closed());

        assert_eq!(
            tracker.settle(
                &mut coordinator,
                nack_result(
                    &update,
                    RenderApplicationNackReason::ApplicationFailure {
                        stage: codec::RenderApplicationStage::ApplySurface,
                    },
                ),
                1_010,
            ),
            RenderApplicationSettleOutcome::CoordinatorDiverged(
                CoordinatorSettleOutcome::StaleOrDuplicate
            )
        );
        assert!(tracker.is_closed());
        assert!(coordinator.is_closed());
        assert_eq!(coordinator.capacity(), HardenedCapacity::default());
    }

    #[test]
    fn application_failure_retries_with_a_new_attempt_without_extending_deadline() {
        let mut coordinator = coordinator();
        let first_claim = claimed_render(&mut coordinator, 9);
        let first_update = render_application_update(&first_claim, 1, 3, 100);
        let first_attempt = first_update.identity.token.attempt;
        let mut tracker = RenderApplicationSettlementTracker::new();
        tracker
            .begin(&first_claim, &first_update, 1_000)
            .expect("first application should bind");
        assert_eq!(
            tracker.settle(
                &mut coordinator,
                nack_result(
                    &first_update,
                    RenderApplicationNackReason::ApplicationFailure {
                        stage: codec::RenderApplicationStage::ApplySurface,
                    },
                ),
                1_010,
            ),
            RenderApplicationSettleOutcome::Retried
        );

        let second_claim = coordinator
            .claim_next()
            .expect("retry claim identity remains available")
            .expect("retry returns the plan to ready");
        let second_update = render_application_update(&second_claim, 2, 3, 89);
        assert_ne!(second_update.identity.token.attempt, first_attempt);
        tracker
            .begin(&second_claim, &second_update, 1_011)
            .expect("second attempt may shorten the original deadline");
        assert_eq!(
            tracker.settle(&mut coordinator, applied_result(&second_update), 1_050),
            RenderApplicationSettleOutcome::Acknowledged
        );
        assert_eq!(tracker.counters().retries, 1);
    }

    #[test]
    fn ready_retry_expires_at_the_original_deadline_and_reclaims_capacity() {
        let mut coordinator = coordinator();
        let first_claim = claimed_render(&mut coordinator, 9);
        let first_update = render_application_update(&first_claim, 1, 3, 100);
        let mut tracker = RenderApplicationSettlementTracker::new();
        tracker
            .begin(&first_claim, &first_update, 1_000)
            .expect("first application should bind");
        assert_eq!(
            tracker.settle(
                &mut coordinator,
                nack_result(
                    &first_update,
                    RenderApplicationNackReason::ApplicationFailure {
                        stage: codec::RenderApplicationStage::ApplySurface,
                    },
                ),
                1_010,
            ),
            RenderApplicationSettleOutcome::Retried
        );
        assert_eq!(tracker.deadline_millis(), Some(1_100));
        assert_eq!(coordinator.capacity().ready_plans, 1);
        assert_eq!(
            tracker.expire_deadline(&mut coordinator, 1_099),
            None
        );
        assert_eq!(
            tracker.expire_deadline(&mut coordinator, 1_100),
            Some(RenderApplicationSettleOutcome::DeadlineExpired {
                coordinator: CoordinatorSettleOutcome::FailedClosed,
            })
        );
        assert!(tracker.is_closed());
        assert!(coordinator.is_closed());
        assert_eq!(coordinator.capacity(), HardenedCapacity::default());
        assert_eq!(tracker.counters().deadline_expirations, 1);
    }

    #[test]
    fn retry_cannot_reset_ordinal_or_extend_the_original_deadline() {
        let mut coordinator = coordinator();
        let first_claim = claimed_render(&mut coordinator, 9);
        let first_update = render_application_update(&first_claim, 1, 3, 100);
        let mut tracker = RenderApplicationSettlementTracker::new();
        tracker
            .begin(&first_claim, &first_update, 1_000)
            .expect("first application should bind");
        assert_eq!(
            tracker.settle(
                &mut coordinator,
                nack_result(
                    &first_update,
                    RenderApplicationNackReason::ApplicationFailure {
                        stage: codec::RenderApplicationStage::ApplySurface,
                    },
                ),
                1_010,
            ),
            RenderApplicationSettleOutcome::Retried
        );

        let second_claim = coordinator
            .claim_next()
            .expect("retry claim identity remains available")
            .expect("retry returns the plan to ready");
        let extended = render_application_update(&second_claim, 2, 3, 100);
        assert_eq!(
            tracker.begin(&second_claim, &extended, 1_011),
            Err(RenderApplicationBeginError::RetryDeadlineExtended)
        );
        assert_eq!(coordinator.capacity().in_flight_plans, 1);
    }

    #[test]
    fn resync_nack_requeues_exact_authority_and_fences_next_attempt_to_snapshot() {
        let mut coordinator = coordinator();
        let claim = claimed_render(&mut coordinator, 9);
        let update = render_application_update(&claim, 1, 3, 100);
        let mut tracker = RenderApplicationSettlementTracker::new();
        tracker
            .begin(&claim, &update, 1_000)
            .expect("exact render claim should bind");
        let reason = RenderApplicationNackReason::BaseMismatch;
        assert_eq!(
            tracker.settle(&mut coordinator, nack_result(&update, reason), 1_010),
            RenderApplicationSettleOutcome::AuthoritativeResyncScheduled { reason }
        );
        assert_eq!(coordinator.capacity().in_flight_plans, 0);
        assert_eq!(coordinator.capacity().ready_plans, 1);
        assert_eq!(tracker.pending_identity(), None);
        assert_eq!(tracker.counters().resync_requests, 1);
        assert_eq!(tracker.counters().retries, 1);

        let snapshot_claim = coordinator
            .claim_next()
            .expect("resync claim identity remains available")
            .expect("resync returns the exact authority to ready");
        let mut snapshot = render_application_update(&snapshot_claim, 2, 3, 89);
        snapshot.identity.kind = RenderApplicationKind::Snapshot;
        snapshot.identity.base_state = None;
        let snapshot_seqno = snapshot.surface.seqno;
        snapshot.surface.bonus_lines = codec::SerializedLines::from(
            (0isize..24)
                .map(|row| (row, wezterm_term::Line::with_width(80, snapshot_seqno)))
                .collect::<Vec<_>>(),
        );
        snapshot.semantic_zones =
            codec::RenderComponentUpdate::Replace(codec::GetSemanticZonesResponse {
                pane_id: snapshot.identity.pane_id,
                zones: Vec::new(),
                zone_texts: Vec::new(),
                last_exit_code: None,
            });
        snapshot.palette = codec::RenderComponentUpdate::Replace(codec::SetPalette {
            pane_id: snapshot.identity.pane_id,
            palette: wezterm_term::color::ColorPalette::default(),
        });
        tracker
            .begin(&snapshot_claim, &snapshot, 1_011)
            .expect("the next exact attempt must accept an authoritative snapshot");
        assert_eq!(
            tracker.settle(&mut coordinator, applied_result(&snapshot), 1_050),
            RenderApplicationSettleOutcome::Acknowledged
        );
        assert_eq!(coordinator.capacity(), HardenedCapacity::default());
    }

    #[test]
    fn pane_scoped_application_cannot_settle_a_global_resync_manifest() {
        let mut coordinator = coordinator();
        let global_resync = DeliveryPlan::new(vec![PlannedEffect::RenderResync {
            authority: ExternalRenderAuthority::new_for_test(
                GENERATION,
                1,
                7,
                1,
                DeliveryScope::ResyncAll,
                1,
            ),
            retained_bytes: 1,
        }])
        .expect("global resync authority is a valid coordinator plan");
        assert!(matches!(
            coordinator.admit_plan(global_resync),
            PlanAdmission::Admitted { .. }
        ));
        let claim = coordinator
            .claim_next()
            .expect("global resync claim identity remains available")
            .expect("global resync plan should be claimable");
        let update = render_application_update(&claim, 1, 3, 100);
        let mut tracker = RenderApplicationSettlementTracker::new();
        assert_eq!(
            tracker.begin(&claim, &update, 1_000),
            Err(RenderApplicationBeginError::ClaimMismatch(
                RenderApplicationClaimMismatch::GlobalResyncRequiresManifest,
            ))
        );
        assert_eq!(coordinator.capacity().in_flight_plans, 1);
        assert!(matches!(
            tracker.close(&mut coordinator),
            RenderApplicationCloseOutcome::Closed(CoordinatorCloseOutcome::Closed(_))
        ));
    }

    #[test]
    fn terminal_nack_retry_exhaustion_and_deadline_expiry_fail_closed() {
        let mut terminal_coordinator = coordinator();
        let terminal_claim = claimed_render(&mut terminal_coordinator, 9);
        let terminal_update = render_application_update(&terminal_claim, 1, 3, 100);
        let mut terminal_tracker = RenderApplicationSettlementTracker::new();
        terminal_tracker
            .begin(&terminal_claim, &terminal_update, 1_000)
            .expect("terminal test application should bind");
        let terminal_reason = RenderApplicationNackReason::UnsupportedResource {
            resource: codec::RenderApplicationResource::Images,
        };
        assert_eq!(
            terminal_tracker.settle(
                &mut terminal_coordinator,
                nack_result(&terminal_update, terminal_reason),
                1_010,
            ),
            RenderApplicationSettleOutcome::FailedClosed {
                reason: terminal_reason,
                coordinator: CoordinatorSettleOutcome::FailedClosed,
            }
        );
        assert!(terminal_coordinator.is_closed());

        let mut exhausted_coordinator = coordinator();
        let exhausted_claim = claimed_render(&mut exhausted_coordinator, 9);
        let exhausted_update = render_application_update(&exhausted_claim, 1, 1, 100);
        let mut exhausted_tracker = RenderApplicationSettlementTracker::new();
        exhausted_tracker
            .begin(&exhausted_claim, &exhausted_update, 1_000)
            .expect("exhaustion test application should bind");
        let retry_reason = RenderApplicationNackReason::ApplicationFailure {
            stage: codec::RenderApplicationStage::ApplySurface,
        };
        assert_eq!(
            exhausted_tracker.settle(
                &mut exhausted_coordinator,
                nack_result(&exhausted_update, retry_reason),
                1_010,
            ),
            RenderApplicationSettleOutcome::RetryExhausted {
                reason: retry_reason,
                coordinator: CoordinatorSettleOutcome::FailedClosed,
            }
        );
        assert!(exhausted_coordinator.is_closed());

        let mut deadline_coordinator = coordinator();
        let deadline_claim = claimed_render(&mut deadline_coordinator, 9);
        let deadline_update = render_application_update(&deadline_claim, 1, 3, 10);
        let mut deadline_tracker = RenderApplicationSettlementTracker::new();
        deadline_tracker
            .begin(&deadline_claim, &deadline_update, 1_000)
            .expect("deadline test application should bind");
        assert_eq!(
            deadline_tracker.expire_deadline(&mut deadline_coordinator, 1_010),
            Some(RenderApplicationSettleOutcome::DeadlineExpired {
                coordinator: CoordinatorSettleOutcome::FailedClosed,
            })
        );
        assert!(deadline_coordinator.is_closed());
    }

    #[test]
    fn disconnect_closes_pending_application_and_reclaims_all_capacity() {
        let mut coordinator = coordinator();
        let claim = claimed_render(&mut coordinator, 9);
        let update = render_application_update(&claim, 1, 3, 100);
        let mut tracker = RenderApplicationSettlementTracker::new();
        tracker
            .begin(&claim, &update, 1_000)
            .expect("exact render claim should bind");
        assert!(matches!(
            tracker.close(&mut coordinator),
            RenderApplicationCloseOutcome::Closed(CoordinatorCloseOutcome::Closed(_))
        ));
        assert!(tracker.is_closed());
        assert!(coordinator.is_closed());
        assert_eq!(coordinator.capacity(), HardenedCapacity::default());
    }

    proptest! {
        #[test]
        fn any_single_wire_identity_corruption_cannot_ack_live_work(field in 0u8..9) {
            let mut coordinator = coordinator();
            let claim = claimed_render(&mut coordinator, 9);
            let update = render_application_update(&claim, 1, 3, 100);
            let mut tracker = RenderApplicationSettlementTracker::new();
            tracker
                .begin(&claim, &update, 1_000)
                .expect("exact render claim should bind");
            let mut corrupted = applied_result(&update);
            match field {
                0 => {
                    corrupted.connection_identity = RenderConnectionIdentity::new(
                        codec::TopologyStreamId::from_bytes([0x99; 16]),
                        mux::MuxSessionIncarnation::from_bytes([0x9a; 16]),
                    );
                }
                1 => corrupted.identity.token.connection_generation += 1,
                2 => corrupted.identity.token.coordinator_instance += 1,
                3 => corrupted.identity.token.scheduler_sequence += 1,
                4 => corrupted.identity.token.attempt += 1,
                5 => corrupted.identity.token.ledger_instance += 1,
                6 => corrupted.identity.token.render_generation += 1,
                7 => corrupted.identity.token.ledger_obligation += 1,
                8 => corrupted.identity.pane_id += 1,
                _ => unreachable!("generated field is constrained to 0..9"),
            }
            prop_assert!(matches!(
                tracker.settle(&mut coordinator, corrupted, 1_010),
                RenderApplicationSettleOutcome::Rejected(_)
            ));
            prop_assert_eq!(coordinator.capacity().in_flight_plans, 1);
            prop_assert_eq!(
                tracker.settle(&mut coordinator, applied_result(&update), 1_020),
                RenderApplicationSettleOutcome::Acknowledged
            );
        }
    }

    #[test]
    fn external_render_authority_preserves_ledger_instance() {
        let mut ledger = crate::delivery_ledger::DeliveryLedger::new(
            crate::delivery_ledger::DeliveryGeneration::new(73),
            1,
        )
        .expect("test delivery-ledger instance allocation should succeed");
        assert_eq!(
            ledger.mark_dirty(9),
            crate::delivery_ledger::DirtyOutcome::BecameDirty
        );
        let claim = ledger
            .claim_next()
            .expect("claim identity allocation should succeed")
            .expect("dirty pane should produce a claim");
        let authority = ExternalRenderAuthority::from_delivery_claim(GENERATION, claim, 11);

        assert_eq!(authority.scheduler_generation(), GENERATION);
        assert_eq!(authority.ledger_instance(), ledger.instance());
        assert_eq!(authority.ledger_generation(), ledger.generation().get());
        assert_eq!(authority.ledger_obligation(), claim.token().sequence());
        assert_eq!(authority.ledger_scope(), claim.scope());
        assert_eq!(authority.source_version(), 11);
    }

    #[test]
    fn render_plan_rejects_claim_scope_or_pane_key_mismatch() {
        let mut coordinator = coordinator();
        let pane_as_resync = DeliveryPlan::new(vec![PlannedEffect::RenderAuthority {
            key: StableEffectKey::new(9, 1),
            claim: ExternalRenderAuthority::new_for_test(
                GENERATION,
                1,
                73,
                1,
                DeliveryScope::ResyncAll,
                1,
            ),
            telemetry_through: None,
            retained_bytes: 1,
        }])
        .expect("scope mismatch is representable before coordinator validation");
        assert!(matches!(
            coordinator.admit_plan(pane_as_resync),
            PlanAdmission::Rejected {
                reason: PlanRejectReason::RenderScopeMismatch,
                ..
            }
        ));

        let wrong_pane_key = DeliveryPlan::new(vec![PlannedEffect::RenderAuthority {
            key: StableEffectKey::new(9, 1),
            claim: ExternalRenderAuthority::new_for_test(
                GENERATION,
                1,
                73,
                2,
                DeliveryScope::Pane(10),
                2,
            ),
            telemetry_through: None,
            retained_bytes: 1,
        }])
        .expect("pane-key mismatch is representable before coordinator validation");
        assert!(matches!(
            coordinator.admit_plan(wrong_pane_key),
            PlanAdmission::Rejected {
                reason: PlanRejectReason::RenderScopeMismatch,
                ..
            }
        ));

        let resync_as_pane = DeliveryPlan::new(vec![PlannedEffect::RenderResync {
            authority: ExternalRenderAuthority::new_for_test(
                GENERATION,
                1,
                73,
                3,
                DeliveryScope::Pane(9),
                3,
            ),
            retained_bytes: 1,
        }])
        .expect("resync-scope mismatch is representable before coordinator validation");
        assert!(matches!(
            coordinator.admit_plan(resync_as_pane),
            PlanAdmission::Rejected {
                reason: PlanRejectReason::RenderScopeMismatch,
                ..
            }
        ));
        assert_eq!(coordinator.capacity(), HardenedCapacity::default());
    }

    fn claim(coordinator: &mut DeliveryCoordinator) -> CoordinatorDeliveryClaim {
        coordinator
            .claim_next()
            .expect("test token space is available")
            .expect("expected an eligible plan")
    }

    fn mutate_coordinator_under_owner<T>(
        coordinator: &mut DeliveryCoordinator,
        mutation: impl FnOnce(&mut DeliveryCoordinator) -> T,
    ) -> (T, Option<PendingDeliveryWake>) {
        // This exclusive borrow models the live external mutex guard. Returning
        // the pending wake ends that guard boundary; tests must wake afterward.
        let outcome = mutation(coordinator);
        let pending_wake = coordinator.take_pending_wake();
        (outcome, pending_wake)
    }

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

    #[test]
    fn claim_retains_authority_until_ack_and_retry_uses_a_new_attempt() {
        let mut coordinator = coordinator();
        assert!(matches!(
            coordinator.admit_plan(event_plan(1, 4)),
            PlanAdmission::Admitted {
                sequence: 1,
                epoch: 0
            }
        ));
        let first = claim(&mut coordinator);
        assert_eq!(
            coordinator.capacity(),
            HardenedCapacity {
                lane_slots_used: [0, 0, 0, 0, 0, 1, 0, 0],
                charged_slots: 1,
                charged_resident_bytes: plan_resident_bytes(1, 4),
                semantic_effects: 1,
                in_flight_plans: 1,
                ..HardenedCapacity::default()
            }
        );
        assert_eq!(
            coordinator.retry(first.token()),
            CoordinatorSettleOutcome::Retried
        );
        let second = claim(&mut coordinator);
        assert_ne!(first.token().attempt(), second.token().attempt());
        assert_eq!(
            coordinator.acknowledge(first.token()),
            CoordinatorSettleOutcome::StaleOrDuplicate,
            "late ACK from attempt one must not settle attempt two"
        );
        assert_eq!(coordinator.capacity().in_flight_plans, 1);
        assert_eq!(
            coordinator.acknowledge(second.token()),
            CoordinatorSettleOutcome::Acknowledged
        );
        assert_eq!(coordinator.capacity(), HardenedCapacity::default());
        assert_eq!(
            coordinator.counters(),
            HardenedCounters {
                plans_admitted: 1,
                claims: 2,
                acknowledgements: 1,
                retries: 1,
                stale_settlements: 1,
                ..HardenedCounters::default()
            }
        );
    }

    #[test]
    fn lifecycle_barrier_cannot_overtake_older_state_or_be_overtaken() {
        let mut coordinator = coordinator();
        assert!(matches!(
            coordinator.admit_plan(state_plan(1)),
            PlanAdmission::Admitted {
                sequence: 1,
                epoch: 0
            }
        ));
        assert!(matches!(
            coordinator.admit_plan(barrier_plan(2)),
            PlanAdmission::Admitted {
                sequence: 2,
                epoch: 0
            }
        ));
        assert!(matches!(
            coordinator.admit_plan(state_plan(3)),
            PlanAdmission::Admitted {
                sequence: 3,
                epoch: 1
            }
        ));

        for (sequence, epoch) in [(1, 0), (2, 0), (3, 1)] {
            let next = claim(&mut coordinator);
            assert_eq!((next.sequence(), next.epoch()), (sequence, epoch));
            assert_eq!(
                coordinator.acknowledge(next.token()),
                CoordinatorSettleOutcome::Acknowledged
            );
        }
    }

    #[test]
    fn lifecycle_barrier_must_be_an_epoch_isolated_plan() {
        let mixed = DeliveryPlan::new(vec![
            PlannedEffect::LifecycleBarrier {
                scope: BarrierScope::Global,
                journal: handle(1, 1),
                retained_bytes: 1,
            },
            PlannedEffect::StateAuthority {
                key: StableEffectKey::new(2, 1),
                authority: handle(2, 1),
                retained_bytes: 1,
            },
        ])
        .expect("the plan builder checks shape overflow, not scheduler causality");
        let mut coordinator = coordinator();
        assert!(matches!(
            coordinator.admit_plan(mixed),
            PlanAdmission::Rejected {
                reason: PlanRejectReason::BarrierMustBeIsolated,
                ..
            }
        ));
        assert_eq!(coordinator.capacity(), HardenedCapacity::default());
    }

    #[test]
    fn invisible_reservation_charges_capacity_and_fences_later_work() {
        let mut coordinator = coordinator();
        let barrier = barrier_plan(1);
        let token = match coordinator.reserve_plan(&barrier) {
            ReservationAdmission::Reserved(token) => token,
            other => panic!("expected reservation, got {other:?}"),
        };
        assert_eq!(coordinator.capacity().reserved_plans, 1);
        assert_eq!(coordinator.capacity().semantic_effects, 0);
        assert!(matches!(
            coordinator.admit_plan(state_plan(2)),
            PlanAdmission::Admitted {
                sequence: 2,
                epoch: 1
            }
        ));
        assert_eq!(
            coordinator.claim_next(),
            Ok(None),
            "later visible state must not pass an invisible reservation"
        );
        assert!(matches!(
            coordinator.commit_reservation(token, barrier),
            ReservationCommit::Committed {
                sequence: 1,
                epoch: 0
            }
        ));
        let first = claim(&mut coordinator);
        assert_eq!(first.sequence(), 1);
        assert_eq!(
            coordinator.acknowledge(first.token()),
            CoordinatorSettleOutcome::Acknowledged,
        );
        assert_eq!(claim(&mut coordinator).sequence(), 2);
    }

    #[test]
    fn lifecycle_reservation_cancellation_preserves_epoch_truth() {
        let mut coordinator_under_test = coordinator();
        let barrier = barrier_plan(1);
        let token = match coordinator_under_test.reserve_plan(&barrier) {
            ReservationAdmission::Reserved(token) => token,
            other => panic!("expected reservation, got {other:?}"),
        };
        assert!(matches!(
            coordinator_under_test.admit_plan(state_plan(2)),
            PlanAdmission::Admitted {
                sequence: 2,
                epoch: 1
            }
        ));
        let before = coordinator_under_test.capacity();
        assert!(matches!(
            coordinator_under_test.cancel_reservation(token),
            ReservationCancel::CausalDependents { .. }
        ));
        assert_eq!(coordinator_under_test.capacity(), before);
        assert_eq!(
            coordinator_under_test
                .counters()
                .reservation_cancel_rejections,
            1
        );

        let mut tail = coordinator();
        let tail_barrier = barrier_plan(3);
        let tail_token = match tail.reserve_plan(&tail_barrier) {
            ReservationAdmission::Reserved(token) => token,
            other => panic!("expected tail reservation, got {other:?}"),
        };
        assert_eq!(
            tail.cancel_reservation(tail_token),
            ReservationCancel::Cancelled
        );
        assert!(matches!(
            tail.admit_plan(state_plan(4)),
            PlanAdmission::Admitted {
                sequence: 2,
                epoch: 0
            }
        ));
    }

    #[test]
    fn large_reservation_set_supports_indexed_cancel_commit_and_slot_reuse() {
        const PLANS: usize = 4096;
        let resident_bytes_per_plan = plan_resident_bytes(1, 1);
        let limits = HardenedSchedulerLimits::new(
            [PLANS; HARDENED_DELIVERY_LANE_COUNT],
            PLANS,
            resident_bytes_per_plan
                .checked_mul(PLANS)
                .expect("large test resident-byte limit is representable"),
            1,
            1,
            PLANS,
        )
        .expect("large indexed-reservation limits are valid");
        let mut coordinator = DeliveryCoordinator::new(GENERATION, limits);
        let mut tokens = Vec::with_capacity(PLANS);

        for index in 0..PLANS {
            let plan = event_plan(u64::try_from(index).expect("test index is representable"), 1);
            let token = match coordinator.reserve_plan(&plan) {
                ReservationAdmission::Reserved(token) => token,
                other => panic!("expected large reservation admission, got {other:?}"),
            };
            tokens.push(Some(token));
        }
        assert_eq!(coordinator.queue.len(), PLANS);
        assert_eq!(coordinator.queue_len, PLANS);
        assert_eq!(coordinator.reservations.len(), PLANS);
        assert_eq!(coordinator.check_invariants(), Ok(()));

        for index in (1..PLANS).step_by(2) {
            let token = tokens[index]
                .take()
                .expect("odd reservation token remains available");
            assert_eq!(
                coordinator.cancel_reservation(token),
                ReservationCancel::Cancelled
            );
        }
        assert_eq!(coordinator.queue_len, PLANS / 2);
        assert_eq!(coordinator.queue_free.len(), PLANS / 2);
        assert_eq!(coordinator.check_invariants(), Ok(()));

        for index in (0..PLANS / 2).rev().map(|position| position * 2) {
            let token = tokens[index]
                .take()
                .expect("even reservation token remains available");
            let plan = event_plan(u64::try_from(index).expect("test index is representable"), 1);
            assert!(matches!(
                coordinator.commit_reservation(token, plan),
                ReservationCommit::Committed { .. }
            ));
        }
        assert!(coordinator.reservations.is_empty());
        assert_eq!(coordinator.check_invariants(), Ok(()));

        for index in (0..PLANS).step_by(2) {
            let next = claim(&mut coordinator);
            assert_eq!(
                next.sequence(),
                u64::try_from(index + 1).expect("test sequence is representable")
            );
            assert_eq!(
                coordinator.acknowledge(next.token()),
                CoordinatorSettleOutcome::Acknowledged
            );
        }
        assert_eq!(coordinator.queue_len, 0);
        assert_eq!(coordinator.queue.len(), PLANS);
        assert_eq!(coordinator.queue_free.len(), PLANS);

        for index in 0..PLANS {
            assert!(matches!(
                coordinator.admit_plan(event_plan(
                    u64::try_from(PLANS + index).expect("test id is representable"),
                    1,
                )),
                PlanAdmission::Admitted { .. }
            ));
        }
        assert_eq!(
            coordinator.queue.len(),
            PLANS,
            "re-admission must reuse stable slots instead of growing storage"
        );
        for _ in 0..PLANS {
            let next = claim(&mut coordinator);
            assert_eq!(
                coordinator.acknowledge(next.token()),
                CoordinatorSettleOutcome::Acknowledged
            );
        }
        assert_eq!(coordinator.capacity(), HardenedCapacity::default());
        assert_eq!(coordinator.check_invariants(), Ok(()));
    }

    #[test]
    fn reservation_rejection_and_mismatch_never_partially_publish() {
        let limits = HardenedSchedulerLimits::new(
            [1; HARDENED_DELIVERY_LANE_COUNT],
            1,
            plan_resident_bytes(1, 1),
            2,
            8,
            1,
        )
        .expect("one-slot limits are valid");
        let mut coordinator = DeliveryCoordinator::new(GENERATION, limits);
        let first = barrier_plan(1);
        let token = match coordinator.reserve_plan(&first) {
            ReservationAdmission::Reserved(token) => token,
            other => panic!("expected first reservation, got {other:?}"),
        };
        let before = coordinator.capacity();
        assert_eq!(
            coordinator.reserve_plan(&barrier_plan(2)),
            ReservationAdmission::Rejected(PlanRejectReason::Capacity)
        );
        assert_eq!(coordinator.capacity(), before);

        let mismatched = event_plan(10, 1);
        let returned_token = match coordinator.commit_reservation(token, mismatched) {
            ReservationCommit::Rejected {
                token,
                reason: PlanRejectReason::FootprintMismatch,
                ..
            } => token,
            other => panic!("expected footprint rejection, got {other:?}"),
        };
        assert_eq!(coordinator.capacity(), before);
        assert_eq!(
            coordinator.cancel_reservation(returned_token),
            ReservationCancel::Cancelled
        );
    }

    #[test]
    fn reservation_capabilities_reject_replay_and_cross_instance_routing() {
        let mut coordinator = coordinator();
        let shape = event_plan(1, 1);
        let token = match coordinator.reserve_plan(&shape) {
            ReservationAdmission::Reserved(token) => token,
            other => panic!("expected reservation, got {other:?}"),
        };
        assert!(matches!(
            coordinator.commit_reservation(token.clone(), event_plan(2, 1)),
            ReservationCommit::Committed {
                sequence: 1,
                epoch: 0
            }
        ));
        assert!(matches!(
            coordinator.commit_reservation(token.clone(), event_plan(3, 1)),
            ReservationCommit::Stale { .. }
        ));
        let invalid_replay = DeliveryPlan::new(vec![PlannedEffect::EventJournal {
            journal: AuthorityHandle::new(
                SchedulerGeneration::for_test(CONNECTION_IDENTITY, 99, 99),
                9,
                1,
            ),
            retained_bytes: 1,
        }])
        .expect("generation validation belongs to the coordinator");
        assert!(matches!(
            coordinator.commit_reservation(token.clone(), invalid_replay),
            ReservationCommit::Stale { .. }
        ));
        assert_eq!(
            coordinator.cancel_reservation(token.clone()),
            ReservationCancel::Stale
        );

        let mut next_instance = DeliveryCoordinator::new(GENERATION, broad_limits());
        let replacement_token = match next_instance.reserve_plan(&event_plan(4, 1)) {
            ReservationAdmission::Reserved(token) => token,
            other => panic!("expected replacement reservation, got {other:?}"),
        };
        let live_origin_token = match coordinator.reserve_plan(&event_plan(40, 1)) {
            ReservationAdmission::Reserved(token) => token,
            other => panic!("expected live origin reservation, got {other:?}"),
        };
        let live_origin_token =
            match next_instance.commit_reservation(live_origin_token, event_plan(41, 1)) {
                ReservationCommit::WrongInstance { token, .. } => token,
                other => panic!("expected recoverable wrong-instance routing, got {other:?}"),
            };
        assert!(matches!(
            coordinator.commit_reservation(live_origin_token, event_plan(42, 1)),
            ReservationCommit::Committed {
                sequence: 2,
                epoch: 0
            }
        ));
        let returned = match next_instance.commit_reservation(token.clone(), event_plan(5, 1)) {
            ReservationCommit::WrongInstance { token, .. } => token,
            other => panic!("expected wrong-instance commit rejection, got {other:?}"),
        };
        assert_eq!(returned, token);
        let returned = match next_instance.cancel_reservation(token.clone()) {
            ReservationCancel::WrongInstance { token } => token,
            other => panic!("expected wrong-instance cancellation rejection, got {other:?}"),
        };
        assert_eq!(returned, token);
        assert!(matches!(
            next_instance.commit_reservation(replacement_token, event_plan(6, 1)),
            ReservationCommit::Committed {
                sequence: 1,
                epoch: 0
            }
        ));
    }

    #[test]
    fn multi_effect_plan_is_all_or_nothing_across_lane_capacity() {
        let limits = HardenedSchedulerLimits::new(
            [1, 1, 1, 1, 1, 0, 1, 1],
            7,
            plan_resident_bytes(2, 2),
            4,
            16,
            8,
        )
        .expect("lane-specific limits are valid");
        let mut coordinator = DeliveryCoordinator::new(GENERATION, limits);
        let plan = DeliveryPlan::new(vec![
            PlannedEffect::StateAuthority {
                key: StableEffectKey::new(1, 1),
                authority: handle(1, 1),
                retained_bytes: 1,
            },
            PlannedEffect::EventJournal {
                journal: handle(2, 1),
                retained_bytes: 1,
            },
        ])
        .expect("state plus event is structurally valid");
        assert!(matches!(
            coordinator.admit_plan(plan),
            PlanAdmission::Rejected {
                reason: PlanRejectReason::Capacity,
                ..
            }
        ));
        assert_eq!(coordinator.capacity(), HardenedCapacity::default());
    }

    #[test]
    fn telemetry_fold_must_precede_dependent_render_in_the_atomic_plan() {
        let accumulator = handle(70, 5);
        let render = PlannedEffect::RenderAuthority {
            key: StableEffectKey::new(7, 1),
            claim: ExternalRenderAuthority::new_for_test(
                GENERATION,
                1,
                1,
                7,
                DeliveryScope::Pane(7),
                8,
            ),
            telemetry_through: Some(TelemetryFence::new(accumulator, 5)),
            retained_bytes: 1,
        };
        let fold = PlannedEffect::TelemetryFold {
            key: StableEffectKey::new(7, 1),
            accumulator,
            through_version: 5,
            retained_bytes: 1,
        };
        let mut coordinator = coordinator();
        let reversed = DeliveryPlan::new(vec![render.clone(), fold.clone()])
            .expect("reversed plan is structurally representable");
        assert!(matches!(
            coordinator.admit_plan(reversed),
            PlanAdmission::Rejected {
                reason: PlanRejectReason::TelemetryDependencyOrder,
                ..
            }
        ));
        let wrong_scope = DeliveryPlan::new(vec![
            PlannedEffect::TelemetryFold {
                key: StableEffectKey::new(8, 1),
                accumulator,
                through_version: 5,
                retained_bytes: 1,
            },
            render.clone(),
        ])
        .expect("cross-scope telemetry is structurally representable");
        assert!(matches!(
            coordinator.admit_plan(wrong_scope),
            PlanAdmission::Rejected {
                reason: PlanRejectReason::TelemetryDependencyOrder,
                ..
            }
        ));
        let masked_valid_fold = DeliveryPlan::new(vec![
            PlannedEffect::TelemetryFold {
                key: StableEffectKey::new(8, 1),
                accumulator,
                through_version: 5,
                retained_bytes: 1,
            },
            fold.clone(),
            render.clone(),
        ])
        .expect("an unrelated fold before the exact dependency is structurally representable");
        assert!(
            matches!(
                coordinator.admit_plan(masked_valid_fold),
                PlanAdmission::Admitted { .. }
            ),
            "an earlier wrong-key fold must not mask a later exact ordered dependency"
        );
        let ordered =
            DeliveryPlan::new(vec![fold, render]).expect("ordered telemetry plan is valid");
        assert!(matches!(
            coordinator.admit_plan(ordered),
            PlanAdmission::Admitted { .. }
        ));
    }

    #[test]
    fn explicit_resident_byte_budget_bounds_large_external_commands() {
        let resident_limit = plan_resident_bytes(1, 4);
        assert!(
            resident_limit > 4,
            "the byte charge includes structural storage, not just caller metadata"
        );
        let limits = HardenedSchedulerLimits::new(
            [2; HARDENED_DELIVERY_LANE_COUNT],
            16,
            resident_limit,
            2,
            4,
            4,
        )
        .expect("byte test limits are valid");
        let mut coordinator = DeliveryCoordinator::new(GENERATION, limits);
        let spooled = DeliveryPlan::new(vec![PlannedEffect::Spool {
            spool: handle(1, 1),
            payload_bytes: u64::MAX,
            retained_bytes: 4,
        }])
        .expect("external payload size does not enlarge the scheduler handle");
        assert!(matches!(
            coordinator.admit_plan(spooled),
            PlanAdmission::Admitted { .. }
        ));
        assert_eq!(
            coordinator.capacity().charged_resident_bytes,
            resident_limit
        );

        let too_large = DeliveryPlan::new(vec![PlannedEffect::Spool {
            spool: handle(2, 1),
            payload_bytes: 1,
            retained_bytes: 5,
        }])
        .expect("plan builder does not own scheduler-specific byte policy");
        assert!(matches!(
            coordinator.admit_plan(too_large),
            PlanAdmission::Rejected {
                reason: PlanRejectReason::EffectTooLarge,
                ..
            }
        ));
        assert!(matches!(
            coordinator.admit_plan(event_plan(3, 1)),
            PlanAdmission::Rejected {
                reason: PlanRejectReason::ResidentBytes,
                ..
            }
        ));
        assert_eq!(
            coordinator.capacity().charged_resident_bytes,
            resident_limit
        );
    }

    #[test]
    fn render_claims_remain_distinct_external_ledger_authorities() {
        let mut coordinator = coordinator();
        assert!(matches!(
            coordinator.admit_plan(render_plan(1)),
            PlanAdmission::Admitted { .. },
        ));
        let successor = DeliveryPlan::new(vec![PlannedEffect::RenderAuthority {
            key: StableEffectKey::new(1, 1),
            claim: ExternalRenderAuthority::new_for_test(
                GENERATION,
                1,
                7,
                2,
                DeliveryScope::Pane(1),
                2,
            ),
            telemetry_through: None,
            retained_bytes: 1,
        }])
        .expect("second external render claim is valid");
        assert!(matches!(
            coordinator.admit_plan(successor),
            PlanAdmission::Admitted { .. },
        ));
        assert_eq!(
            coordinator.capacity().lane_slots_used[AuthorityLane::Render.index()],
            2
        );
        let first = claim(&mut coordinator);
        assert_eq!(
            coordinator.acknowledge(first.token()),
            CoordinatorSettleOutcome::Acknowledged,
        );
        let second = claim(&mut coordinator);
        assert_ne!(
            first.plan().effects(),
            second.plan().effects(),
            "external ledger claims are never replaced by scheduler key coalescing"
        );
    }

    #[test]
    fn atomic_poll_registration_wakes_on_admission_and_terminal_close() {
        let mut coordinator = coordinator();
        let wake_counter = Arc::new(WakeCounter::default());
        let waker = Waker::from(Arc::clone(&wake_counter));
        let mut context = Context::from_waker(&waker);
        assert_eq!(coordinator.poll_claim_next(&mut context), Poll::Pending);
        assert_eq!(
            coordinator.claim_next(),
            Ok(None),
            "an empty direct probe must not erase a registered waiter"
        );
        let (admission, pending_wake) = mutate_coordinator_under_owner(&mut coordinator, |owned| {
            owned.admit_plan(event_plan(1, 1))
        });
        assert!(matches!(admission, PlanAdmission::Admitted { .. }));
        assert_eq!(
            wake_counter.0.load(Ordering::Relaxed),
            0,
            "admission must not invoke an arbitrary waker under its owner guard"
        );
        pending_wake
            .expect("claimable admission must defer the registered wake")
            .wake();
        assert_eq!(wake_counter.0.load(Ordering::Relaxed), 1);
        let in_flight = match coordinator.poll_claim_next(&mut context) {
            Poll::Ready(Ok(Some(claim))) => claim,
            other => panic!("expected ready claim, got {other:?}"),
        };
        assert_eq!(coordinator.poll_claim_next(&mut context), Poll::Pending);
        let (outcome, pending_wake) = mutate_coordinator_under_owner(&mut coordinator, |owned| {
            owned.nack_terminal(in_flight.token())
        });
        assert_eq!(outcome, CoordinatorSettleOutcome::FailedClosed);
        assert_eq!(
            wake_counter.0.load(Ordering::Relaxed),
            1,
            "terminal close must defer its wake until after owner release"
        );
        pending_wake
            .expect("terminal close must extract the registered waiter")
            .wake();
        assert_eq!(
            wake_counter.0.load(Ordering::Relaxed),
            2,
            "terminal closure must wake a consumer parked behind in-flight work"
        );
    }

    #[test]
    fn waiter_wakes_only_when_the_causal_head_becomes_claimable() {
        let mut coordinator = coordinator();
        let reserved = event_plan(1, 1);
        let token = match coordinator.reserve_plan(&reserved) {
            ReservationAdmission::Reserved(token) => token,
            other => panic!("expected head reservation, got {other:?}"),
        };
        let wake_counter = Arc::new(WakeCounter::default());
        let waker = Waker::from(Arc::clone(&wake_counter));
        let mut context = Context::from_waker(&waker);
        assert_eq!(coordinator.poll_claim_next(&mut context), Poll::Pending);
        let (admission, pending_wake) = mutate_coordinator_under_owner(&mut coordinator, |owned| {
            owned.admit_plan(event_plan(2, 1))
        });
        assert!(matches!(admission, PlanAdmission::Admitted { .. }));
        assert!(
            pending_wake.is_none(),
            "a successor fenced by the head reservation must retain its waiter"
        );
        assert_eq!(
            wake_counter.0.load(Ordering::Relaxed),
            0,
            "successor admission behind a reservation is not claimable"
        );
        let (commit, pending_wake) = mutate_coordinator_under_owner(&mut coordinator, |owned| {
            owned.commit_reservation(token, reserved)
        });
        assert!(matches!(commit, ReservationCommit::Committed { .. }));
        assert_eq!(
            wake_counter.0.load(Ordering::Relaxed),
            0,
            "reservation commit must defer its claimable-head wake"
        );
        pending_wake
            .expect("claimable reservation commit must extract the waiter")
            .wake();
        assert_eq!(wake_counter.0.load(Ordering::Relaxed), 1);

        let first = claim(&mut coordinator);
        assert_eq!(coordinator.poll_claim_next(&mut context), Poll::Pending);
        let (outcome, pending_wake) = mutate_coordinator_under_owner(&mut coordinator, |owned| {
            owned.acknowledge(first.token())
        });
        assert_eq!(outcome, CoordinatorSettleOutcome::Acknowledged);
        assert_eq!(
            wake_counter.0.load(Ordering::Relaxed),
            1,
            "head acknowledgement must defer its successor wake"
        );
        pending_wake
            .expect("claimable successor must extract the waiter")
            .wake();
        assert_eq!(wake_counter.0.load(Ordering::Relaxed), 2);

        let second = claim(&mut coordinator);
        assert_eq!(coordinator.poll_claim_next(&mut context), Poll::Pending);
        let (outcome, pending_wake) = mutate_coordinator_under_owner(&mut coordinator, |owned| {
            owned.acknowledge(second.token())
        });
        assert_eq!(outcome, CoordinatorSettleOutcome::Acknowledged);
        assert!(
            pending_wake.is_none(),
            "settlement to an empty queue must retain its waiter"
        );
        assert_eq!(
            wake_counter.0.load(Ordering::Relaxed),
            2,
            "settlement to an empty queue preserves the waiter without a wake"
        );
        let (admission, pending_wake) = mutate_coordinator_under_owner(&mut coordinator, |owned| {
            owned.admit_plan(event_plan(3, 1))
        });
        assert!(matches!(admission, PlanAdmission::Admitted { .. }));
        assert_eq!(
            wake_counter.0.load(Ordering::Relaxed),
            2,
            "later admission must defer the retained waiter"
        );
        pending_wake
            .expect("later claimable admission must extract the waiter")
            .wake();
        assert_eq!(
            wake_counter.0.load(Ordering::Relaxed),
            3,
            "the preserved waiter observes the next claimable admission"
        );
    }

    #[test]
    fn close_report_separates_reserved_slots_from_semantic_effects() {
        let mut coordinator = coordinator();
        let reserved = event_plan(1, 3);
        let _token = match coordinator.reserve_plan(&reserved) {
            ReservationAdmission::Reserved(token) => token,
            other => panic!("expected reservation, got {other:?}"),
        };
        let semantic = DeliveryPlan::new(vec![
            PlannedEffect::StateAuthority {
                key: StableEffectKey::new(2, 1),
                authority: handle(2, 1),
                retained_bytes: 5,
            },
            PlannedEffect::EventJournal {
                journal: handle(3, 1),
                retained_bytes: 7,
            },
        ])
        .expect("two-effect plan is valid");
        assert!(matches!(
            coordinator.admit_plan(semantic),
            PlanAdmission::Admitted { .. },
        ));
        assert_eq!(
            coordinator.close(),
            CoordinatorCloseOutcome::Closed(CoordinatorCloseReport {
                reserved_plans: 1,
                ready_plans: 1,
                in_flight_plans: 0,
                semantic_effects: 2,
                charged_slots: 3,
                charged_resident_bytes: plan_resident_bytes(1, 3) + plan_resident_bytes(2, 12),
            })
        );
        assert_eq!(coordinator.capacity(), HardenedCapacity::default());
        assert_eq!(coordinator.counters().closed_semantic_effects, 2);
        assert_eq!(coordinator.counters().closed_charged_slots, 3);
    }

    #[test]
    fn wrong_generation_authority_and_stale_claim_fail_closed_or_reject() {
        let mut coordinator = coordinator();
        let wrong = DeliveryPlan::new(vec![PlannedEffect::EventJournal {
            journal: AuthorityHandle::new(
                SchedulerGeneration::for_test(CONNECTION_IDENTITY, 99, 99),
                1,
                1,
            ),
            retained_bytes: 1,
        }])
        .expect("wrong generation is scheduler validation, not plan shape");
        assert!(matches!(
            coordinator.admit_plan(wrong),
            PlanAdmission::Rejected {
                reason: PlanRejectReason::WrongGeneration,
                ..
            }
        ));

        assert!(matches!(
            coordinator.admit_plan(event_plan(2, 1)),
            PlanAdmission::Admitted { .. },
        ));
        let old = claim(&mut coordinator);
        let mut replacement = DeliveryCoordinator::new(GENERATION, broad_limits());
        assert!(matches!(
            replacement.admit_plan(event_plan(3, 1)),
            PlanAdmission::Admitted { .. },
        ));
        let current = claim(&mut replacement);
        assert_eq!(
            replacement.acknowledge(old.token()),
            CoordinatorSettleOutcome::WrongInstance
        );
        assert_eq!(
            replacement.acknowledge(current.token()),
            CoordinatorSettleOutcome::Acknowledged
        );
        assert!(!replacement.is_closed());
    }

    #[test]
    fn attempt_token_exhaustion_closes_without_false_acknowledgement() {
        let mut coordinator = coordinator();
        coordinator.set_next_attempt_for_test(None);
        assert!(matches!(
            coordinator.admit_plan(event_plan(1, 1)),
            PlanAdmission::Admitted { .. },
        ));
        assert_eq!(
            coordinator.claim_next(),
            Err(CoordinatorClaimError::TokenExhausted)
        );
        assert!(coordinator.is_closed());
        assert_eq!(
            coordinator.last_close_report(),
            Some(CoordinatorCloseReport {
                ready_plans: 1,
                semantic_effects: 1,
                charged_slots: 1,
                charged_resident_bytes: plan_resident_bytes(1, 1),
                ..CoordinatorCloseReport::default()
            })
        );
    }

    #[test]
    fn attempt_token_exhaustion_preserves_registered_wake_for_owner_delivery() {
        let mut coordinator = coordinator();
        let wake_counter = Arc::new(WakeCounter::default());
        let waker = Waker::from(Arc::clone(&wake_counter));
        let mut context = Context::from_waker(&waker);
        assert_eq!(coordinator.poll_claim_next(&mut context), Poll::Pending);
        coordinator.set_next_attempt_for_test(None);
        assert!(matches!(
            coordinator.admit_plan(event_plan(1, 1)),
            PlanAdmission::Admitted { .. },
        ));
        assert!(
            coordinator.pending_wake.is_some(),
            "claimable admission should defer the registered wake",
        );

        // Model an owner that probes before extracting and delivering the
        // deferred wake. Exhaustion must not erase that wake obligation.
        assert_eq!(
            coordinator.claim_next(),
            Err(CoordinatorClaimError::TokenExhausted),
        );
        assert!(coordinator.is_closed());
        coordinator
            .take_pending_wake()
            .expect("exhaustion must preserve the deferred wake")
            .wake();
        assert_eq!(wake_counter.0.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn closed_effect_enum_charges_every_authority_lane_exactly_once() {
        let barrier = DeliveryPlan::new(vec![PlannedEffect::LifecycleBarrier {
            scope: BarrierScope::Key(StableEffectKey::new(1, 1)),
            journal: handle(1, 1),
            retained_bytes: 1,
        }])
        .expect("isolated lifecycle authority is valid");
        let remaining_lanes = DeliveryPlan::new(vec![
            PlannedEffect::StateResync {
                authority: ResyncAuthority::new(handle(2, 2), 20),
                retained_bytes: 2,
            },
            PlannedEffect::RenderResync {
                authority: ExternalRenderAuthority::new_for_test(
                    GENERATION,
                    1,
                    3,
                    30,
                    DeliveryScope::ResyncAll,
                    30,
                ),
                retained_bytes: 3,
            },
            PlannedEffect::TopologyResync {
                authority: ResyncAuthority::new(handle(4, 4), 40),
                retained_bytes: 4,
            },
            PlannedEffect::AuxiliaryResync {
                authority: ResyncAuthority::new(handle(5, 5), 50),
                retained_bytes: 5,
            },
            PlannedEffect::EventJournal {
                journal: handle(6, 6),
                retained_bytes: 6,
            },
            PlannedEffect::TelemetryFold {
                key: StableEffectKey::new(7, 1),
                accumulator: handle(7, 7),
                through_version: 7,
                retained_bytes: 7,
            },
            PlannedEffect::Spool {
                spool: handle(8, 8),
                payload_bytes: 8_000_000,
                retained_bytes: 8,
            },
        ])
        .expect("one authority per non-lifecycle lane is a valid plan");
        let mut coordinator = coordinator();
        assert!(matches!(
            coordinator.admit_plan(barrier),
            PlanAdmission::Admitted { .. }
        ));
        assert!(matches!(
            coordinator.admit_plan(remaining_lanes),
            PlanAdmission::Admitted { .. }
        ));
        assert_eq!(
            coordinator.capacity(),
            HardenedCapacity {
                lane_slots_used: [1; HARDENED_DELIVERY_LANE_COUNT],
                charged_slots: HARDENED_DELIVERY_LANE_COUNT,
                charged_resident_bytes: plan_resident_bytes(1, 1) + plan_resident_bytes(7, 35),
                semantic_effects: HARDENED_DELIVERY_LANE_COUNT,
                ready_plans: 2,
                ..HardenedCapacity::default()
            }
        );
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum ReferenceState {
        Reserved,
        Ready,
        InFlight { attempt: u64 },
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum ReferenceCancel {
        Cancelled,
        CausalDependents,
        Stale,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct ReferenceEntry {
        sequence: u64,
        epoch: u64,
        lane: AuthorityLane,
        state: ReferenceState,
    }

    #[derive(Debug, Default)]
    struct ReferenceCounters {
        plans_admitted: u64,
        plans_rejected: u64,
        reservations_admitted: u64,
        reservations_rejected: u64,
        reservations_committed: u64,
        reservations_cancelled: u64,
        reservation_cancel_rejections: u64,
        reservation_commit_rejections: u64,
        stale_reservations: u64,
        claims: u64,
        acknowledgements: u64,
        retries: u64,
        terminal_nacks: u64,
        stale_settlements: u64,
        closes: u64,
        closed_semantic_effects: u64,
        closed_charged_slots: u64,
        closed_resident_bytes: u64,
    }

    #[derive(Debug)]
    struct ReferenceCoordinator {
        queue: VecDeque<ReferenceEntry>,
        next_sequence: u64,
        next_attempt: u64,
        epoch: u64,
        closed: bool,
        counters: ReferenceCounters,
        event_limit: usize,
        lifecycle_limit: usize,
        total_limit: usize,
        resident_bytes_per_plan: usize,
    }

    fn reference_increment(value: &mut u64) {
        *value = value.saturating_add(1);
    }

    fn reference_add_usize(value: &mut u64, addend: usize) {
        *value = value.saturating_add(u64::try_from(addend).unwrap_or(u64::MAX));
    }

    impl ReferenceCoordinator {
        fn new(
            event_limit: usize,
            lifecycle_limit: usize,
            total_limit: usize,
            resident_bytes_per_plan: usize,
        ) -> Self {
            Self {
                queue: VecDeque::new(),
                next_sequence: 1,
                next_attempt: 1,
                epoch: 0,
                closed: false,
                counters: ReferenceCounters::default(),
                event_limit,
                lifecycle_limit,
                total_limit,
                resident_bytes_per_plan,
            }
        }

        fn lane_count(&self, lane: AuthorityLane) -> usize {
            self.queue.iter().filter(|entry| entry.lane == lane).count()
        }

        fn has_capacity(&self, lane: AuthorityLane) -> bool {
            let lane_limit = match lane {
                AuthorityLane::Lifecycle => self.lifecycle_limit,
                AuthorityLane::Event => self.event_limit,
                _ => 0,
            };
            self.queue.len() < self.total_limit && self.lane_count(lane) < lane_limit
        }

        fn admit(&mut self, lane: AuthorityLane) -> Option<(u64, u64)> {
            if self.closed {
                return None;
            }
            if !self.has_capacity(lane) {
                reference_increment(&mut self.counters.plans_rejected);
                return None;
            }
            let sequence = self.next_sequence;
            self.next_sequence += 1;
            let epoch = self.epoch;
            self.queue.push_back(ReferenceEntry {
                sequence,
                epoch,
                lane,
                state: ReferenceState::Ready,
            });
            if lane == AuthorityLane::Lifecycle {
                self.epoch += 1;
            }
            reference_increment(&mut self.counters.plans_admitted);
            Some((sequence, epoch))
        }

        fn reserve(&mut self, lane: AuthorityLane) -> Option<(u64, u64)> {
            if self.closed {
                return None;
            }
            if !self.has_capacity(lane) {
                reference_increment(&mut self.counters.reservations_rejected);
                return None;
            }
            let sequence = self.next_sequence;
            self.next_sequence += 1;
            let epoch = self.epoch;
            self.queue.push_back(ReferenceEntry {
                sequence,
                epoch,
                lane,
                state: ReferenceState::Reserved,
            });
            if lane == AuthorityLane::Lifecycle {
                self.epoch += 1;
            }
            reference_increment(&mut self.counters.reservations_admitted);
            Some((sequence, epoch))
        }

        fn commit(&mut self, sequence: u64) -> bool {
            let Some(entry) = self
                .queue
                .iter_mut()
                .find(|entry| entry.sequence == sequence)
            else {
                return false;
            };
            if entry.state != ReferenceState::Reserved {
                return false;
            }
            entry.state = ReferenceState::Ready;
            reference_increment(&mut self.counters.reservations_committed);
            true
        }

        fn reject_commit(&mut self, sequence: u64) -> bool {
            if self.closed
                || !self.queue.iter().any(|entry| {
                    entry.sequence == sequence && entry.state == ReferenceState::Reserved
                })
            {
                return false;
            }
            reference_increment(&mut self.counters.reservation_commit_rejections);
            true
        }

        fn reject_wrong_generation_admission(&mut self) -> bool {
            if self.closed {
                return false;
            }
            reference_increment(&mut self.counters.plans_rejected);
            true
        }

        fn reject_wrong_generation_reservation(&mut self) -> bool {
            if self.closed {
                return false;
            }
            reference_increment(&mut self.counters.reservations_rejected);
            true
        }

        fn reject_stale_reservation(&mut self) -> bool {
            if self.closed {
                return false;
            }
            reference_increment(&mut self.counters.stale_reservations);
            true
        }

        fn cancel(&mut self, sequence: u64) -> ReferenceCancel {
            let Some(position) = self.queue.iter().position(|entry| {
                entry.sequence == sequence && entry.state == ReferenceState::Reserved
            }) else {
                return ReferenceCancel::Stale;
            };
            let entry = self.queue[position];
            if entry.lane == AuthorityLane::Lifecycle && position + 1 != self.queue.len() {
                reference_increment(&mut self.counters.reservation_cancel_rejections);
                return ReferenceCancel::CausalDependents;
            }
            let Some(_removed) = self.queue.remove(position) else {
                unreachable!("reference reservation remains present during cancellation")
            };
            if entry.lane == AuthorityLane::Lifecycle {
                self.epoch = entry.epoch;
            }
            reference_increment(&mut self.counters.reservations_cancelled);
            ReferenceCancel::Cancelled
        }

        fn claim(&mut self) -> Option<(u64, u64, u64)> {
            let front = self.queue.front_mut()?;
            if front.state != ReferenceState::Ready || self.closed {
                return None;
            }
            let attempt = self.next_attempt;
            self.next_attempt += 1;
            front.state = ReferenceState::InFlight { attempt };
            reference_increment(&mut self.counters.claims);
            Some((front.sequence, front.epoch, attempt))
        }

        fn acknowledge(&mut self, sequence: u64, attempt: u64) -> bool {
            let matches = self.queue.front().is_some_and(|entry| {
                entry.sequence == sequence && entry.state == ReferenceState::InFlight { attempt }
            });
            if !matches {
                reference_increment(&mut self.counters.stale_settlements);
                return false;
            }
            let Some(_acknowledged) = self.queue.pop_front() else {
                unreachable!("reference matching claim remains present during acknowledgement")
            };
            reference_increment(&mut self.counters.acknowledgements);
            true
        }

        fn retry(&mut self, sequence: u64, attempt: u64) -> bool {
            let Some(front) = self.queue.front_mut() else {
                reference_increment(&mut self.counters.stale_settlements);
                return false;
            };
            if front.sequence != sequence || front.state != (ReferenceState::InFlight { attempt }) {
                reference_increment(&mut self.counters.stale_settlements);
                return false;
            }
            front.state = ReferenceState::Ready;
            reference_increment(&mut self.counters.retries);
            true
        }

        fn reject_stale_settlement(&mut self) -> bool {
            if self.closed {
                return false;
            }
            reference_increment(&mut self.counters.stale_settlements);
            true
        }

        fn nack_terminal(&mut self, sequence: u64, attempt: u64) -> bool {
            if self.closed {
                return false;
            }
            let matches = self.queue.front().is_some_and(|entry| {
                entry.sequence == sequence && entry.state == ReferenceState::InFlight { attempt }
            });
            if !matches {
                reference_increment(&mut self.counters.stale_settlements);
                return false;
            }
            reference_increment(&mut self.counters.terminal_nacks);
            self.close();
            true
        }

        fn close(&mut self) {
            if self.closed {
                return;
            }
            let semantic = self
                .queue
                .iter()
                .filter(|entry| entry.state != ReferenceState::Reserved)
                .count();
            let slots = self.queue.len();
            self.queue.clear();
            self.closed = true;
            reference_increment(&mut self.counters.closes);
            reference_add_usize(&mut self.counters.closed_semantic_effects, semantic);
            reference_add_usize(&mut self.counters.closed_charged_slots, slots);
            reference_add_usize(
                &mut self.counters.closed_resident_bytes,
                slots * self.resident_bytes_per_plan,
            );
        }
    }

    proptest! {
        #[test]
        fn independent_reference_model_matches_order_capacity_and_counters(
            event_limit in 0usize..=4,
            lifecycle_limit in 0usize..=4,
            total_limit in 1usize..=6,
            actions in prop::collection::vec((0u8..=7, any::<u16>()), 0..160),
        ) {
            let lane_slots = [
                lifecycle_limit,
                0,
                0,
                0,
                0,
                event_limit,
                0,
                0,
            ];
            let aggregate = lifecycle_limit + event_limit;
            let effective_total = total_limit.min(aggregate);
            prop_assume!(effective_total > 0);
            let resident_bytes_per_plan = plan_resident_bytes(1, 1);
            let limits = HardenedSchedulerLimits::new(
                lane_slots,
                effective_total,
                effective_total * resident_bytes_per_plan,
                1,
                1,
                effective_total,
            ).expect("generated limits are representable");
            let mut production = DeliveryCoordinator::new(GENERATION, limits);
            let mut reference = ReferenceCoordinator::new(
                event_limit,
                lifecycle_limit,
                effective_total,
                resident_bytes_per_plan,
            );
            let mut reservations: Vec<(ReservationToken, u64, AuthorityLane)> = Vec::new();
            let mut active_claim: Option<(CoordinatorClaimToken, u64, u64)> = None;

            for (action, value) in actions {
                let id = u64::from(value) + 1;
                match action {
                    0 | 1 => {
                        let lane = if action == 0 {
                            AuthorityLane::Event
                        } else {
                            AuthorityLane::Lifecycle
                        };
                        let plan = if lane == AuthorityLane::Event {
                            event_plan(id, 1)
                        } else {
                            barrier_plan(id)
                        };
                        let expected = reference.admit(lane);
                        let actual = production.admit_plan(plan);
                        match (expected, actual) {
                            (Some((sequence, epoch)), PlanAdmission::Admitted {
                                sequence: actual_sequence,
                                epoch: actual_epoch,
                            }) => {
                                prop_assert_eq!((actual_sequence, actual_epoch), (sequence, epoch));
                            }
                            (
                                None,
                                PlanAdmission::Rejected {
                                    reason: PlanRejectReason::Capacity,
                                    ..
                                }
                                | PlanAdmission::Closed { .. },
                            ) => {}
                            pair => prop_assert!(false, "admission mismatch: {pair:?}"),
                        }
                    }
                    2 => {
                        let lane = if value % 2 == 0 {
                            AuthorityLane::Event
                        } else {
                            AuthorityLane::Lifecycle
                        };
                        let plan = if lane == AuthorityLane::Event {
                            event_plan(id, 1)
                        } else {
                            barrier_plan(id)
                        };
                        let expected = reference.reserve(lane);
                        let actual = production.reserve_plan(&plan);
                        match (expected, actual) {
                            (Some((sequence, _)), ReservationAdmission::Reserved(token)) => {
                                prop_assert_eq!(token.sequence(), sequence);
                                reservations.push((token, sequence, lane));
                            }
                            (
                                None,
                                ReservationAdmission::Rejected(PlanRejectReason::Capacity)
                                | ReservationAdmission::Closed,
                            ) => {}
                            pair => prop_assert!(false, "reservation mismatch: {pair:?}"),
                        }
                    }
                    3 => {
                        if let Some((token, sequence, lane)) = reservations.pop() {
                            let plan = if lane == AuthorityLane::Event {
                                event_plan(id, 1)
                            } else {
                                barrier_plan(id)
                            };
                            let expected = reference.commit(sequence);
                            let actual = production.commit_reservation(token, plan);
                            prop_assert_eq!(
                                matches!(actual, ReservationCommit::Committed { .. }),
                                expected
                            );
                        }
                    }
                    4 => {
                        if let Some((token, sequence, lane)) = reservations.pop() {
                            let expected = reference.cancel(sequence);
                            let actual = production.cancel_reservation(token);
                            let actual = match actual {
                                ReservationCancel::Cancelled => ReferenceCancel::Cancelled,
                                ReservationCancel::CausalDependents { token } => {
                                    reservations.push((token, sequence, lane));
                                    ReferenceCancel::CausalDependents
                                }
                                ReservationCancel::Stale
                                | ReservationCancel::WrongInstance { .. }
                                | ReservationCancel::GenerationClosed => {
                                    ReferenceCancel::Stale
                                }
                            };
                            prop_assert_eq!(actual, expected);
                        }
                    }
                    5 => {
                        let expected = reference.claim();
                        let actual = production.claim_next().expect("small trace cannot exhaust tokens");
                        match (expected, actual) {
                            (Some((sequence, epoch, attempt)), Some(claim)) => {
                                prop_assert_eq!(
                                    (claim.sequence(), claim.epoch(), claim.token().attempt()),
                                    (sequence, epoch, attempt)
                                );
                                active_claim = Some((claim.token().clone(), sequence, attempt));
                            }
                            (None, None) => {}
                            pair => prop_assert!(false, "claim mismatch: {pair:?}"),
                        }
                    }
                    6 => {
                        if let Some((token, sequence, attempt)) = active_claim.take() {
                            let expected = reference.acknowledge(sequence, attempt);
                            let actual = production.acknowledge(&token);
                            prop_assert_eq!(
                                actual == CoordinatorSettleOutcome::Acknowledged,
                                expected
                            );
                        }
                    }
                    7 => {
                        if let Some((token, sequence, attempt)) = active_claim.take() {
                            let expected = reference.retry(sequence, attempt);
                            let actual = production.retry(&token);
                            prop_assert_eq!(actual == CoordinatorSettleOutcome::Retried, expected);
                        } else {
                            reference.close();
                            let _ = production.close();
                        }
                    }
                    _ => unreachable!("generated action is constrained to 0..=7"),
                }

                let capacity = production.capacity();
                let reference_reserved = reference
                    .queue
                    .iter()
                    .filter(|entry| entry.state == ReferenceState::Reserved)
                    .count();
                let reference_ready = reference
                    .queue
                    .iter()
                    .filter(|entry| entry.state == ReferenceState::Ready)
                    .count();
                let reference_in_flight = reference
                    .queue
                    .iter()
                    .filter(|entry| matches!(entry.state, ReferenceState::InFlight { .. }))
                    .count();
                prop_assert_eq!(capacity.charged_slots, reference.queue.len());
                prop_assert_eq!(
                    capacity.charged_resident_bytes,
                    reference.queue.len() * resident_bytes_per_plan
                );
                prop_assert_eq!(capacity.reserved_plans, reference_reserved);
                prop_assert_eq!(capacity.ready_plans, reference_ready);
                prop_assert_eq!(capacity.in_flight_plans, reference_in_flight);
                prop_assert_eq!(
                    capacity.semantic_effects,
                    reference_ready + reference_in_flight
                );
                let counters = production.counters();
                prop_assert_eq!(counters.plans_admitted, reference.counters.plans_admitted);
                prop_assert_eq!(counters.plans_rejected, reference.counters.plans_rejected);
                prop_assert_eq!(
                    counters.reservations_admitted,
                    reference.counters.reservations_admitted
                );
                prop_assert_eq!(
                    counters.reservations_rejected,
                    reference.counters.reservations_rejected
                );
                prop_assert_eq!(
                    counters.reservations_committed,
                    reference.counters.reservations_committed
                );
                prop_assert_eq!(
                    counters.reservations_cancelled,
                    reference.counters.reservations_cancelled
                );
                prop_assert_eq!(
                    counters.reservation_cancel_rejections,
                    reference.counters.reservation_cancel_rejections
                );
                prop_assert_eq!(
                    counters.reservation_commit_rejections,
                    reference.counters.reservation_commit_rejections
                );
                prop_assert_eq!(
                    counters.stale_reservations,
                    reference.counters.stale_reservations
                );
                prop_assert_eq!(counters.claims, reference.counters.claims);
                prop_assert_eq!(
                    counters.acknowledgements,
                    reference.counters.acknowledgements
                );
                prop_assert_eq!(counters.retries, reference.counters.retries);
                prop_assert_eq!(
                    counters.terminal_nacks,
                    reference.counters.terminal_nacks
                );
                prop_assert_eq!(counters.closes, reference.counters.closes);
                prop_assert_eq!(
                    counters.closed_semantic_effects,
                    reference.counters.closed_semantic_effects
                );
                prop_assert_eq!(
                    counters.closed_charged_slots,
                    reference.counters.closed_charged_slots
                );
                prop_assert_eq!(
                    counters.closed_resident_bytes,
                    reference.counters.closed_resident_bytes
                );
                prop_assert_eq!(
                    counters.stale_settlements,
                    reference.counters.stale_settlements
                );
                prop_assert_eq!(production.check_invariants(), Ok(()));
            }
        }

        #[test]
        fn generated_negative_trace_exercises_replay_rejection_and_terminal_nack(
            use_event_lane in any::<bool>(),
            seed in any::<u64>(),
        ) {
            let lane = if use_event_lane {
                AuthorityLane::Event
            } else {
                AuthorityLane::Lifecycle
            };
            let opposite_lane = if use_event_lane {
                AuthorityLane::Lifecycle
            } else {
                AuthorityLane::Event
            };
            let mut lane_slots = [0usize; HARDENED_DELIVERY_LANE_COUNT];
            lane_slots[lane.index()] = 1;
            let resident_bytes_per_plan = plan_resident_bytes(1, 1);
            let limits = HardenedSchedulerLimits::new(
                lane_slots,
                1,
                resident_bytes_per_plan,
                1,
                1,
                1,
            ).expect("single-lane negative-trace limits are valid");
            let mut production = DeliveryCoordinator::new(GENERATION, limits);
            let mut reference = ReferenceCoordinator::new(
                usize::from(use_event_lane),
                usize::from(!use_event_lane),
                1,
                resident_bytes_per_plan,
            );
            let id = |offset: u64| seed.wrapping_add(offset);

            prop_assert!(reference.reject_wrong_generation_admission());
            prop_assert!(matches!(
                production.admit_plan(wrong_generation_plan(lane, id(1))),
                PlanAdmission::Rejected {
                    reason: PlanRejectReason::WrongGeneration,
                    ..
                }
            ), "wrong-generation admission was not rejected");
            prop_assert!(reference.reject_wrong_generation_reservation());
            prop_assert_eq!(
                production.reserve_plan(&wrong_generation_plan(lane, id(2))),
                ReservationAdmission::Rejected(PlanRejectReason::WrongGeneration)
            );

            let (sequence, epoch) = reference.reserve(lane).expect("lane has one slot");
            let token = match production.reserve_plan(&plan_for_lane(lane, id(3))) {
                ReservationAdmission::Reserved(token) => token,
                other => panic!("expected valid reservation, got {other:?}"),
            };
            prop_assert_eq!((token.sequence(), epoch), (sequence, 0));
            let stale_token = token.clone();

            prop_assert!(reference.reject_commit(sequence));
            let token = match production.commit_reservation(
                token,
                plan_for_lane(opposite_lane, id(4)),
            ) {
                ReservationCommit::Rejected {
                    token,
                    reason: PlanRejectReason::FootprintMismatch,
                    ..
                } => token,
                other => panic!("expected footprint rejection, got {other:?}"),
            };

            prop_assert!(reference.reject_commit(sequence));
            let token = match production.commit_reservation(
                token,
                wrong_generation_plan(lane, id(5)),
            ) {
                ReservationCommit::Rejected {
                    token,
                    reason: PlanRejectReason::WrongGeneration,
                    ..
                } => token,
                other => panic!("expected generation rejection, got {other:?}"),
            };

            prop_assert!(reference.commit(sequence));
            prop_assert!(matches!(
                production.commit_reservation(token, plan_for_lane(lane, id(6))),
                ReservationCommit::Committed {
                    sequence: 1,
                    epoch: 0
                }
            ), "valid reserved plan did not commit");

            prop_assert!(reference.reject_stale_reservation());
            prop_assert!(matches!(
                production.commit_reservation(stale_token, plan_for_lane(lane, id(7))),
                ReservationCommit::Stale { .. }
            ), "stale reservation replay was not rejected");

            let (sequence, epoch, attempt) = reference.claim().expect("committed plan is ready");
            let first = production
                .claim_next()
                .expect("attempt space is available")
                .expect("committed plan is claimable");
            prop_assert_eq!(
                (first.sequence(), first.epoch(), first.token().attempt()),
                (sequence, epoch, attempt)
            );
            let old_attempt = first.token().clone();
            prop_assert!(reference.retry(sequence, attempt));
            prop_assert_eq!(
                production.retry(first.token()),
                CoordinatorSettleOutcome::Retried
            );
            prop_assert!(reference.reject_stale_settlement());
            prop_assert_eq!(
                production.acknowledge(&old_attempt),
                CoordinatorSettleOutcome::StaleOrDuplicate
            );

            let (sequence, epoch, attempt) =
                reference.claim().expect("retried plan is ready");
            let second = production
                .claim_next()
                .expect("attempt space is available")
                .expect("retried plan is claimable");
            prop_assert_eq!(
                (second.sequence(), second.epoch(), second.token().attempt()),
                (sequence, epoch, attempt)
            );
            let duplicate_attempt = second.token().clone();
            prop_assert!(reference.acknowledge(sequence, attempt));
            prop_assert_eq!(
                production.acknowledge(second.token()),
                CoordinatorSettleOutcome::Acknowledged
            );
            prop_assert!(reference.reject_stale_settlement());
            prop_assert_eq!(
                production.retry(&duplicate_attempt),
                CoordinatorSettleOutcome::StaleOrDuplicate
            );

            let (sequence, epoch) = reference.admit(lane).expect("ACK released the slot");
            prop_assert!(matches!(
                production.admit_plan(plan_for_lane(lane, id(8))),
                PlanAdmission::Admitted {
                    sequence: actual_sequence,
                    epoch: actual_epoch,
                } if (actual_sequence, actual_epoch) == (sequence, epoch)
            ), "post-ACK admission did not preserve sequence and epoch");
            let (sequence, epoch, attempt) =
                reference.claim().expect("terminal-NACK plan is ready");
            let terminal = production
                .claim_next()
                .expect("attempt space is available")
                .expect("terminal-NACK plan is claimable");
            prop_assert_eq!(
                (terminal.sequence(), terminal.epoch(), terminal.token().attempt()),
                (sequence, epoch, attempt)
            );
            prop_assert!(reference.nack_terminal(sequence, attempt));
            prop_assert_eq!(
                production.nack_terminal(terminal.token()),
                CoordinatorSettleOutcome::FailedClosed
            );

            let counters = production.counters();
            prop_assert_eq!(counters.plans_rejected, reference.counters.plans_rejected);
            prop_assert_eq!(
                counters.reservations_rejected,
                reference.counters.reservations_rejected
            );
            prop_assert_eq!(
                counters.reservation_commit_rejections,
                reference.counters.reservation_commit_rejections
            );
            prop_assert_eq!(
                counters.stale_reservations,
                reference.counters.stale_reservations
            );
            prop_assert_eq!(
                counters.stale_settlements,
                reference.counters.stale_settlements
            );
            prop_assert_eq!(
                counters.terminal_nacks,
                reference.counters.terminal_nacks
            );
            prop_assert_eq!(counters.closes, reference.counters.closes);
            prop_assert_eq!(counters.plans_rejected, 1);
            prop_assert_eq!(counters.reservations_rejected, 1);
            prop_assert_eq!(counters.reservation_commit_rejections, 2);
            prop_assert_eq!(counters.stale_reservations, 1);
            prop_assert_eq!(counters.stale_settlements, 2);
            prop_assert_eq!(counters.claims, 3);
            prop_assert_eq!(counters.acknowledgements, 1);
            prop_assert_eq!(counters.retries, 1);
            prop_assert_eq!(counters.terminal_nacks, 1);
            prop_assert_eq!(counters.closes, 1);
            prop_assert_eq!(counters.wrong_instance_reservations, 0);
            prop_assert_eq!(counters.wrong_instance_settlements, 0);
            prop_assert_eq!(counters.closed_semantic_effects, 1);
            prop_assert_eq!(counters.closed_charged_slots, 1);
            prop_assert_eq!(
                counters.closed_resident_bytes,
                u64::try_from(resident_bytes_per_plan).unwrap_or(u64::MAX)
            );
            prop_assert_eq!(production.capacity(), HardenedCapacity::default());
            prop_assert_eq!(production.check_invariants(), Ok(()));
        }
    }
}
