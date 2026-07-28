//! Executable bounded scheduler model for mux delivery classes.
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

use crate::delivery_ledger::DELIVERY_PRIORITY_BURST_LIMIT;
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::Hash;

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
struct KeyedLane<K, V> {
    limit: usize,
    values: HashMap<K, V>,
    order: VecDeque<K>,
    resync_pending: bool,
}

impl<K, V> KeyedLane<K, V>
where
    K: Clone + Eq + Hash,
{
    fn new(limit: usize) -> Self {
        Self {
            limit,
            values: HashMap::new(),
            order: VecDeque::new(),
            resync_pending: false,
        }
    }

    fn admit(&mut self, key: K, value: V) -> AdmissionOutcome {
        if self.resync_pending {
            return AdmissionOutcome::Coalesced;
        }

        if let Some(current) = self.values.get_mut(&key) {
            *current = value;
            return AdmissionOutcome::Coalesced;
        }

        if self.values.len() < self.limit {
            self.order.push_back(key.clone());
            let replaced = self.values.insert(key, value);
            debug_assert!(replaced.is_none());
            AdmissionOutcome::Admitted
        } else {
            self.resync_pending = true;
            AdmissionOutcome::Escalated
        }
    }

    fn is_ready(&self) -> bool {
        self.resync_pending || !self.order.is_empty()
    }

    fn pop(&mut self) -> Option<KeyedPop<K, V>> {
        if self.resync_pending {
            self.resync_pending = false;
            self.values.clear();
            self.order.clear();
            return Some(KeyedPop::Resync);
        }

        let key = self.order.pop_front()?;
        let value = self
            .values
            .remove(&key)
            .expect("keyed scheduler order and value map must agree");
        Some(KeyedPop::Value { key, value })
    }

    fn clear(&mut self) {
        self.values.clear();
        self.order.clear();
        self.resync_pending = false;
    }

    fn check_invariants(&self) -> Result<(), &'static str> {
        if self.values.len() > self.limit {
            return Err("keyed lane exceeds its configured key limit");
        }
        if self.order.len() != self.values.len() {
            return Err("keyed lane order and value counts disagree");
        }

        let mut seen = HashSet::with_capacity(self.order.len());
        for key in &self.order {
            if !seen.insert(key) {
                return Err("keyed lane order contains a duplicate stable key");
            }
            if !self.values.contains_key(key) {
                return Err("keyed lane order references a missing value");
            }
        }
        if seen.len() != self.values.len() {
            return Err("keyed lane value map contains an unordered key");
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
            used: self.state.values.len(),
        };
        let state_resync = LaneCapacity {
            limit: 1,
            used: usize::from(self.state.resync_pending),
        };
        let render_keys = LaneCapacity {
            limit: self.limits.render_keys,
            used: self.render.values.len(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    type TestScheduler = DeliveryScheduler<u64, u64, u64, u64, u64, u64>;

    fn scheduler(limits: SchedulerLimits) -> TestScheduler {
        DeliveryScheduler::new(limits).expect("test capacities must be representable")
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

        let classes: Vec<_> = (0..(DELIVERY_PRIORITY_BURST_LIMIT * 3 + 1))
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
        scheduler.admit_lifecycle(0);
        scheduler.admit_state(0, 0);
        scheduler.admit_render(0, 0);
        scheduler.admit_payload(0);

        let mut next_value = 1;
        let mut classes = Vec::new();
        for _ in 0..256 {
            let item = scheduler
                .pop_next()
                .expect("every durable class is replenished after selection");
            let class = item.class();
            classes.push(class);
            match class {
                DurableClass::Lifecycle => {
                    scheduler.admit_lifecycle(next_value);
                }
                DurableClass::State => {
                    scheduler.admit_state(next_value, next_value);
                }
                DurableClass::Render => {
                    scheduler.admit_render(next_value, next_value);
                }
                DurableClass::Payload => {
                    scheduler.admit_payload(next_value);
                }
            }
            next_value += 1;
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
        scheduler.admit_lifecycle(1);
        scheduler.admit_lifecycle(2);
        scheduler.admit_state(1, 1);
        scheduler.admit_state(2, 2);
        scheduler.admit_render(1, 1);
        scheduler.admit_payload(1);
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
