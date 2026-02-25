//! Cancellation-safe channel with reserve/commit semantics.
//!
//! Prevents silent message loss when async tasks are cancelled mid-send.
//! Instead of sending a value directly (which can be lost if the future is
//! dropped during `select!` or cancellation), callers:
//!
//! 1. **Reserve** capacity — returns a `Reservation` token
//! 2. **Commit** the value through the token — atomically places the value
//! 3. Or let the token **drop** — capacity is released, nothing is lost
//!
//! # Architecture
//!
//! ```text
//! Producer                    Channel                   Consumer
//! --------                    -------                   --------
//! reserve() ─────────────→ slot claimed (capacity--)
//! commit(token, val) ────→ value placed ──────────────→ recv()
//!   or
//! drop(token) ───────────→ slot freed (capacity++)
//! ```
//!
//! # Guarantees
//!
//! - **No silent loss**: A message is either committed (consumer will see it)
//!   or the reservation is rolled back (sender is notified via `Reservation::is_committed()`).
//! - **Bounded**: Total capacity is pre-allocated; `reserve()` blocks when full.
//! - **Lock-free fast path**: Uses `crossbeam::queue::ArrayQueue` internally.
//! - **Cancellation-safe**: Dropping a `Reservation` before commit releases
//!   the slot without delivering a value. The sender retains the value.
//!
//! # Use cases
//!
//! - Event bus delivery with guaranteed subscriber consumption
//! - Tailer segment capture with explicit GAP signaling
//! - Mission deconfliction message delivery
//! - Watchdog observation with controlled shedding

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crossbeam::queue::ArrayQueue;
use serde::{Deserialize, Serialize};

use crate::runtime_compat::notify::Notify;

// ── Reservation ────────────────────────────────────────────────────────────

/// A token representing a reserved slot in the channel.
///
/// Must be either committed (via `TxProducer::commit()`) or dropped (auto-rollback).
/// Dropping without committing releases capacity but does not deliver a value.
#[derive(Debug)]
pub struct Reservation {
    seq: u64,
    committed: AtomicBool,
    channel_id: u64,
    /// Shared state for auto-rollback on drop.
    rollback_handle: Arc<RollbackState>,
}

#[derive(Debug)]
struct RollbackState {
    /// Number of active (uncommitted) reservations.
    active_reservations: AtomicU64,
    /// Notify waiters that capacity has been freed.
    capacity_freed: Notify,
}

impl Reservation {
    /// The sequence number of this reservation.
    #[must_use]
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Whether this reservation has been committed.
    #[must_use]
    pub fn is_committed(&self) -> bool {
        self.committed.load(Ordering::Acquire)
    }

    /// The channel this reservation belongs to.
    #[must_use]
    pub fn channel_id(&self) -> u64 {
        self.channel_id
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if !self.committed.load(Ordering::Acquire) {
            // Rollback: release the reserved capacity
            self.rollback_handle
                .active_reservations
                .fetch_sub(1, Ordering::Release);
            self.rollback_handle.capacity_freed.notify_one();
        }
    }
}

// ── Channel Errors ─────────────────────────────────────────────────────────

/// Errors from channel operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxChannelError {
    /// Channel has been closed.
    Closed,
    /// Channel is at capacity (for try_reserve).
    Full,
    /// Reservation belongs to a different channel.
    WrongChannel { expected: u64, actual: u64 },
    /// Reservation was already committed.
    AlreadyCommitted { seq: u64 },
}

impl fmt::Display for TxChannelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => write!(f, "channel closed"),
            Self::Full => write!(f, "channel full"),
            Self::WrongChannel { expected, actual } => {
                write!(f, "reservation for channel {actual}, expected {expected}")
            }
            Self::AlreadyCommitted { seq } => {
                write!(f, "reservation {seq} already committed")
            }
        }
    }
}

// ── Tx Channel ─────────────────────────────────────────────────────────────

/// Shared state for the transactional channel.
struct TxShared<T> {
    /// The underlying lock-free queue for committed values.
    queue: ArrayQueue<TxEnvelope<T>>,
    /// Total capacity (queue capacity + active reservations).
    capacity: usize,
    /// Whether the channel is closed.
    closed: AtomicBool,
    /// Monotonic sequence counter.
    next_seq: AtomicU64,
    /// Unique channel ID for reservation validation.
    channel_id: u64,
    /// Consumer notification.
    not_empty: Notify,
    /// Producer notification (capacity freed).
    rollback_state: Arc<RollbackState>,
}

/// Envelope wrapping a committed value with metadata.
struct TxEnvelope<T> {
    seq: u64,
    value: T,
}

/// Construct a bounded transactional channel.
///
/// # Panics
/// Panics when `capacity == 0`.
pub fn tx_channel<T>(capacity: usize) -> (TxProducer<T>, TxConsumer<T>) {
    assert!(capacity > 0, "TxChannel capacity must be > 0");

    // Use a simple atomic counter for channel IDs
    static NEXT_CHANNEL_ID: AtomicU64 = AtomicU64::new(1);
    let channel_id = NEXT_CHANNEL_ID.fetch_add(1, Ordering::Relaxed);

    let rollback_state = Arc::new(RollbackState {
        active_reservations: AtomicU64::new(0),
        capacity_freed: Notify::new(),
    });

    let shared = Arc::new(TxShared {
        queue: ArrayQueue::new(capacity),
        capacity,
        closed: AtomicBool::new(false),
        next_seq: AtomicU64::new(1),
        channel_id,
        not_empty: Notify::new(),
        rollback_state,
    });

    (
        TxProducer {
            shared: Arc::clone(&shared),
        },
        TxConsumer { shared },
    )
}

// ── Producer ───────────────────────────────────────────────────────────────

/// The producer side of a transactional channel.
pub struct TxProducer<T> {
    shared: Arc<TxShared<T>>,
}

impl<T> TxProducer<T> {
    /// Try to reserve a slot without blocking.
    ///
    /// Returns `Err(Full)` if the channel is at capacity, or `Err(Closed)`
    /// if the channel has been closed.
    pub fn try_reserve(&self) -> Result<Reservation, TxChannelError> {
        loop {
            if self.shared.closed.load(Ordering::Acquire) {
                return Err(TxChannelError::Closed);
            }

            let active = self
                .shared
                .rollback_state
                .active_reservations
                .load(Ordering::Acquire);
            let queued = self.shared.queue.len();
            let used = queued.saturating_add(active as usize);

            if used >= self.shared.capacity {
                return Err(TxChannelError::Full);
            }

            let next_active = match active.checked_add(1) {
                Some(next) => next,
                None => return Err(TxChannelError::Full),
            };

            if self
                .shared
                .rollback_state
                .active_reservations
                .compare_exchange_weak(active, next_active, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                break;
            }

            std::hint::spin_loop();
        }

        let seq = self.shared.next_seq.fetch_add(1, Ordering::Relaxed);

        Ok(Reservation {
            seq,
            committed: AtomicBool::new(false),
            channel_id: self.shared.channel_id,
            rollback_handle: Arc::clone(&self.shared.rollback_state),
        })
    }

    /// Reserve a slot, blocking if the channel is full.
    ///
    /// Returns `Err(Closed)` if the channel is closed while waiting.
    pub async fn reserve(&self) -> Result<Reservation, TxChannelError> {
        loop {
            match self.try_reserve() {
                Ok(r) => return Ok(r),
                Err(TxChannelError::Full) => {
                    // Wait for capacity to be freed
                    self.shared.rollback_state.capacity_freed.notified().await;
                    if self.shared.closed.load(Ordering::Acquire) {
                        return Err(TxChannelError::Closed);
                    }
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Commit a value through a reservation, delivering it to consumers.
    ///
    /// The reservation is consumed and marked as committed. The value is
    /// placed in the channel and consumers are notified.
    pub fn commit(&self, reservation: &Reservation, value: T) -> Result<(), TxChannelError> {
        // Validate reservation
        if reservation.channel_id != self.shared.channel_id {
            return Err(TxChannelError::WrongChannel {
                expected: self.shared.channel_id,
                actual: reservation.channel_id,
            });
        }

        if reservation
            .committed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(TxChannelError::AlreadyCommitted {
                seq: reservation.seq,
            });
        }

        // Place value in queue
        let envelope = TxEnvelope {
            seq: reservation.seq,
            value,
        };

        // With a valid reservation this should not fail. If it does, keep the
        // reservation active by reverting the committed flag; dropping the
        // reservation will release capacity.
        if self.shared.queue.push(envelope).is_err() {
            reservation.committed.store(false, Ordering::Release);
            return Err(TxChannelError::Full);
        }

        // Convert the active reservation slot into a queued item slot.
        self.shared
            .rollback_state
            .active_reservations
            .fetch_sub(1, Ordering::Release);

        // Notify consumers
        self.shared.not_empty.notify_one();
        self.shared.rollback_state.capacity_freed.notify_one();
        Ok(())
    }

    /// Convenience: reserve + commit in one step (non-blocking reserve).
    ///
    /// This is equivalent to `try_reserve()` + `commit()` but slightly
    /// more ergonomic for non-cancellation-sensitive code paths.
    pub fn try_send(&self, value: T) -> Result<u64, TxChannelError> {
        let reservation = self.try_reserve()?;
        let seq = reservation.seq;
        self.commit(&reservation, value)?;
        Ok(seq)
    }

    /// Close the producer side. Consumers will drain remaining values.
    pub fn close(&self) {
        self.shared.closed.store(true, Ordering::Release);
        self.shared.not_empty.notify_waiters();
        self.shared.rollback_state.capacity_freed.notify_waiters();
    }

    /// Whether the channel is closed.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::Acquire)
    }

    /// Current number of committed values in the queue.
    #[must_use]
    pub fn queued(&self) -> usize {
        self.shared.queue.len()
    }

    /// Current number of active (uncommitted) reservations.
    #[must_use]
    pub fn active_reservations(&self) -> u64 {
        self.shared
            .rollback_state
            .active_reservations
            .load(Ordering::Acquire)
    }

    /// Total capacity of the channel.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.shared.capacity
    }

    /// Available capacity (capacity - queued - active_reservations).
    #[must_use]
    pub fn available(&self) -> usize {
        let used = self.shared.queue.len() + self.active_reservations() as usize;
        self.shared.capacity.saturating_sub(used)
    }

    /// The channel's unique ID.
    #[must_use]
    pub fn channel_id(&self) -> u64 {
        self.shared.channel_id
    }
}

impl<T> Clone for TxProducer<T> {
    fn clone(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<T> fmt::Debug for TxProducer<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TxProducer")
            .field("channel_id", &self.shared.channel_id)
            .field("capacity", &self.shared.capacity)
            .field("queued", &self.shared.queue.len())
            .field("active_reservations", &self.active_reservations())
            .field("closed", &self.is_closed())
            .finish()
    }
}

// ── Consumer ───────────────────────────────────────────────────────────────

/// The consumer side of a transactional channel.
pub struct TxConsumer<T> {
    shared: Arc<TxShared<T>>,
}

/// A received value with its sequence number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceivedValue<T> {
    /// The sequence number assigned when the reservation was created.
    pub seq: u64,
    /// The committed value.
    pub value: T,
}

impl<T> TxConsumer<T> {
    /// Try to receive a value without blocking.
    ///
    /// Returns `None` if the queue is empty.
    pub fn try_recv(&self) -> Option<ReceivedValue<T>> {
        self.shared.queue.pop().map(|env| {
            // Notify producer that capacity has been freed
            self.shared.rollback_state.capacity_freed.notify_one();
            ReceivedValue {
                seq: env.seq,
                value: env.value,
            }
        })
    }

    /// Receive a value, blocking if the queue is empty.
    ///
    /// Returns `None` if the channel is closed and all values have been consumed.
    pub async fn recv(&self) -> Option<ReceivedValue<T>> {
        loop {
            if let Some(rv) = self.try_recv() {
                return Some(rv);
            }
            if self.shared.closed.load(Ordering::Acquire) && self.shared.queue.is_empty() {
                return None;
            }
            self.shared.not_empty.notified().await;
        }
    }

    /// Number of values available to receive.
    #[must_use]
    pub fn len(&self) -> usize {
        self.shared.queue.len()
    }

    /// Whether the receive queue is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.shared.queue.is_empty()
    }

    /// Whether the channel is closed.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::Acquire)
    }

    /// The channel's unique ID.
    #[must_use]
    pub fn channel_id(&self) -> u64 {
        self.shared.channel_id
    }
}

impl<T> fmt::Debug for TxConsumer<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TxConsumer")
            .field("channel_id", &self.shared.channel_id)
            .field("queued", &self.shared.queue.len())
            .field("closed", &self.is_closed())
            .finish()
    }
}

// ── Channel Metrics ────────────────────────────────────────────────────────

/// Snapshot of channel metrics for diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TxChannelMetrics {
    pub channel_id: u64,
    pub capacity: usize,
    pub queued: usize,
    pub active_reservations: u64,
    pub available: usize,
    pub closed: bool,
}

impl<T> TxProducer<T> {
    /// Snapshot metrics for diagnostics.
    #[must_use]
    pub fn metrics(&self) -> TxChannelMetrics {
        TxChannelMetrics {
            channel_id: self.shared.channel_id,
            capacity: self.shared.capacity,
            queued: self.shared.queue.len(),
            active_reservations: self.active_reservations(),
            available: self.available(),
            closed: self.is_closed(),
        }
    }
}

// ── Channel Registry ───────────────────────────────────────────────────────

/// Tracks all active transactional channels for observability.
#[derive(Debug, Default)]
pub struct TxChannelRegistry {
    /// Channel names → IDs for named lookups.
    names: HashMap<String, u64>,
    /// Per-channel metadata.
    metadata: HashMap<u64, ChannelMetadata>,
}

/// Metadata about a registered channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelMetadata {
    pub channel_id: u64,
    pub name: String,
    pub capacity: usize,
    pub created_at_ms: i64,
    pub purpose: String,
}

impl TxChannelRegistry {
    /// Create a new empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a channel with a human-readable name and purpose.
    pub fn register(
        &mut self,
        channel_id: u64,
        name: impl Into<String>,
        capacity: usize,
        created_at_ms: i64,
        purpose: impl Into<String>,
    ) {
        let name = name.into();
        let purpose = purpose.into();
        self.names.insert(name.clone(), channel_id);
        self.metadata.insert(
            channel_id,
            ChannelMetadata {
                channel_id,
                name,
                capacity,
                created_at_ms,
                purpose,
            },
        );
    }

    /// Look up a channel by name.
    #[must_use]
    pub fn by_name(&self, name: &str) -> Option<&ChannelMetadata> {
        self.names.get(name).and_then(|id| self.metadata.get(id))
    }

    /// Look up a channel by ID.
    #[must_use]
    pub fn by_id(&self, id: u64) -> Option<&ChannelMetadata> {
        self.metadata.get(&id)
    }

    /// All registered channels.
    #[must_use]
    pub fn all(&self) -> Vec<&ChannelMetadata> {
        self.metadata.values().collect()
    }

    /// Number of registered channels.
    #[must_use]
    pub fn len(&self) -> usize {
        self.metadata.len()
    }

    /// Whether the registry is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.metadata.is_empty()
    }

    /// Deterministic canonical string.
    #[must_use]
    pub fn canonical_string(&self) -> String {
        let mut names: Vec<&String> = self.names.keys().collect();
        names.sort();
        format!(
            "tx_channel_registry|count={}|channels={}",
            self.metadata.len(),
            names
                .iter()
                .map(|n| n.as_str())
                .collect::<Vec<_>>()
                .join(","),
        )
    }
}

// ── Reserve Guard ──────────────────────────────────────────────────────────

/// A guard that holds both a reservation and the value to commit.
///
/// Provides a convenient pattern for code that wants to:
/// 1. Reserve capacity
/// 2. Do some work that might fail or be cancelled
/// 3. Commit or let the guard drop (auto-rollback)
pub struct ReserveGuard<'a, T> {
    producer: &'a TxProducer<T>,
    reservation: Reservation,
    value: Option<T>,
    committed: bool,
}

impl<'a, T> ReserveGuard<'a, T> {
    /// Create a reserve guard with a pre-reserved slot and pending value.
    #[must_use]
    pub fn new(producer: &'a TxProducer<T>, reservation: Reservation, value: T) -> Self {
        Self {
            producer,
            reservation,
            value: Some(value),
            committed: false,
        }
    }

    /// Commit the value through the reservation.
    pub fn commit(mut self) -> Result<u64, TxChannelError> {
        let value = self.value.take().expect("value consumed only once");
        self.producer.commit(&self.reservation, value)?;
        self.committed = true;
        Ok(self.reservation.seq)
    }

    /// Get the sequence number.
    #[must_use]
    pub fn seq(&self) -> u64 {
        self.reservation.seq
    }

    /// Whether this guard has been committed.
    #[must_use]
    pub fn is_committed(&self) -> bool {
        self.committed
    }

    /// Take the value back without committing (explicit rollback).
    pub fn rollback(mut self) -> T {
        self.value.take().expect("value consumed only once")
    }
}

impl<T> Drop for ReserveGuard<'_, T> {
    fn drop(&mut self) {
        // If not committed, the Reservation's Drop will handle rollback.
        // The value (if still held) is just dropped normally.
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::Barrier;
    use std::thread;

    #[test]
    fn basic_reserve_commit() {
        let (tx, rx) = tx_channel::<String>(4);

        let r = tx.try_reserve().unwrap();
        assert_eq!(r.seq(), 1);
        assert!(!r.is_committed());
        assert_eq!(tx.active_reservations(), 1);

        tx.commit(&r, "hello".into()).unwrap();
        assert!(r.is_committed());
        assert_eq!(tx.active_reservations(), 0);
        assert_eq!(tx.queued(), 1);

        let rv = rx.try_recv().unwrap();
        assert_eq!(rv.seq, 1);
        assert_eq!(rv.value, "hello");
    }

    #[test]
    fn rollback_on_drop() {
        let (tx, _rx) = tx_channel::<String>(4);

        {
            let _r = tx.try_reserve().unwrap();
            assert_eq!(tx.active_reservations(), 1);
            // _r drops here — auto-rollback
        }

        assert_eq!(tx.active_reservations(), 0);
        assert_eq!(tx.queued(), 0);
    }

    #[test]
    fn capacity_enforcement() {
        let (tx, _rx) = tx_channel::<u32>(2);

        let r1 = tx.try_reserve().unwrap();
        let r2 = tx.try_reserve().unwrap();
        assert!(matches!(tx.try_reserve(), Err(TxChannelError::Full)));

        // Commit r1 — still full (1 committed + 1 reserved = 2)
        tx.commit(&r1, 10).unwrap();
        assert!(matches!(tx.try_reserve(), Err(TxChannelError::Full)));

        // Drop r2 — frees a slot
        drop(r2);
        assert!(tx.try_reserve().is_ok());
    }

    #[test]
    fn concurrent_try_reserve_respects_capacity() {
        let (tx, _rx) = tx_channel::<u32>(1);
        let workers = 16usize;
        let barrier = Arc::new(Barrier::new(workers));
        let mut handles = Vec::with_capacity(workers);

        for _ in 0..workers {
            let tx_clone = tx.clone();
            let barrier_clone = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                barrier_clone.wait();
                tx_clone.try_reserve().ok()
            }));
        }

        let reservations = handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect::<Vec<_>>();
        let successes = reservations.iter().filter(|res| res.is_some()).count();

        assert_eq!(
            successes, 1,
            "only one reservation should succeed for capacity=1"
        );
        assert_eq!(tx.active_reservations(), 1);
    }

    #[test]
    fn try_send_convenience() {
        let (tx, rx) = tx_channel::<u32>(4);

        let seq = tx.try_send(42).unwrap();
        assert_eq!(seq, 1);

        let rv = rx.try_recv().unwrap();
        assert_eq!(rv.value, 42);
    }

    #[test]
    fn wrong_channel_rejected() {
        let (tx1, _rx1) = tx_channel::<u32>(4);
        let (tx2, _rx2) = tx_channel::<u32>(4);

        let r = tx2.try_reserve().unwrap();
        let result = tx1.commit(&r, 42);
        assert!(matches!(result, Err(TxChannelError::WrongChannel { .. })));
    }

    #[test]
    fn double_commit_rejected() {
        let (tx, _rx) = tx_channel::<u32>(4);
        let r = tx.try_reserve().unwrap();
        tx.commit(&r, 42).unwrap();

        let result = tx.commit(&r, 43);
        assert!(matches!(
            result,
            Err(TxChannelError::AlreadyCommitted { .. })
        ));
    }

    #[test]
    fn close_rejects_new_reservations() {
        let (tx, _rx) = tx_channel::<u32>(4);
        tx.close();

        assert!(matches!(tx.try_reserve(), Err(TxChannelError::Closed)));
    }

    #[test]
    fn consumer_drains_after_close() {
        let (tx, rx) = tx_channel::<u32>(4);
        tx.try_send(1).unwrap();
        tx.try_send(2).unwrap();
        tx.close();

        assert_eq!(rx.try_recv().unwrap().value, 1);
        assert_eq!(rx.try_recv().unwrap().value, 2);
        assert!(rx.try_recv().is_none());
    }

    #[test]
    fn sequence_numbers_monotonic() {
        let (tx, rx) = tx_channel::<u32>(8);

        for i in 0..5 {
            tx.try_send(i).unwrap();
        }

        let mut prev_seq = 0;
        while let Some(rv) = rx.try_recv() {
            assert!(rv.seq > prev_seq, "seq {} should be > {}", rv.seq, prev_seq);
            prev_seq = rv.seq;
        }
    }

    #[test]
    fn metrics_snapshot() {
        let (tx, _rx) = tx_channel::<u32>(8);

        let r = tx.try_reserve().unwrap();
        tx.try_send(1).unwrap();

        let m = tx.metrics();
        assert_eq!(m.capacity, 8);
        assert_eq!(m.queued, 1);
        assert_eq!(m.active_reservations, 1);
        assert_eq!(m.available, 6);
        assert!(!m.closed);

        drop(r);
        let m2 = tx.metrics();
        assert_eq!(m2.active_reservations, 0);
        assert_eq!(m2.available, 7);
    }

    #[test]
    fn reserve_guard_commit() {
        let (tx, rx) = tx_channel::<String>(4);

        let r = tx.try_reserve().unwrap();
        let guard = ReserveGuard::new(&tx, r, "hello".into());
        let seq = guard.commit().unwrap();
        assert_eq!(seq, 1);

        let rv = rx.try_recv().unwrap();
        assert_eq!(rv.value, "hello");
    }

    #[test]
    fn reserve_guard_rollback() {
        let (tx, rx) = tx_channel::<String>(4);

        let r = tx.try_reserve().unwrap();
        let guard = ReserveGuard::new(&tx, r, "hello".into());
        let value = guard.rollback();
        assert_eq!(value, "hello");

        assert!(rx.try_recv().is_none());
        assert_eq!(tx.active_reservations(), 0);
    }

    #[test]
    fn reserve_guard_auto_rollback_on_drop() {
        let (tx, rx) = tx_channel::<String>(4);

        {
            let r = tx.try_reserve().unwrap();
            let _guard = ReserveGuard::new(&tx, r, "hello".into());
            // guard drops without commit
        }

        assert!(rx.try_recv().is_none());
        assert_eq!(tx.active_reservations(), 0);
    }

    #[test]
    fn registry_operations() {
        let mut reg = TxChannelRegistry::new();
        assert!(reg.is_empty());

        reg.register(1, "capture-out", 256, 1000, "capture pipeline output");
        reg.register(2, "event-bus", 1024, 1000, "event bus delivery");

        assert_eq!(reg.len(), 2);
        assert_eq!(reg.by_name("capture-out").unwrap().channel_id, 1);
        assert_eq!(reg.by_id(2).unwrap().name, "event-bus");
        assert!(reg.by_name("nonexistent").is_none());
    }

    #[test]
    fn registry_canonical_string_deterministic() {
        let mut reg = TxChannelRegistry::new();
        reg.register(1, "b-channel", 8, 1000, "b");
        reg.register(2, "a-channel", 8, 1000, "a");

        let s1 = reg.canonical_string();
        let s2 = reg.canonical_string();
        assert_eq!(s1, s2);
        // Names should be sorted alphabetically
        assert!(s1.contains("a-channel,b-channel"));
    }

    #[test]
    fn available_capacity_accounts_for_all() {
        let (tx, rx) = tx_channel::<u32>(4);

        // 4 capacity, 0 used
        assert_eq!(tx.available(), 4);

        // Reserve 1
        let r1 = tx.try_reserve().unwrap();
        assert_eq!(tx.available(), 3);

        // Commit 1 (still occupies queue space)
        tx.commit(&r1, 10).unwrap();
        assert_eq!(tx.available(), 3);

        // Reserve another
        let _r2 = tx.try_reserve().unwrap();
        assert_eq!(tx.available(), 2);

        // Consumer takes one → frees queue space
        rx.try_recv().unwrap();
        assert_eq!(tx.available(), 3);
    }

    #[test]
    fn error_display() {
        assert_eq!(TxChannelError::Closed.to_string(), "channel closed");
        assert_eq!(TxChannelError::Full.to_string(), "channel full");
        assert_eq!(
            TxChannelError::WrongChannel {
                expected: 1,
                actual: 2
            }
            .to_string(),
            "reservation for channel 2, expected 1"
        );
        assert_eq!(
            TxChannelError::AlreadyCommitted { seq: 5 }.to_string(),
            "reservation 5 already committed"
        );
    }

    #[test]
    fn serde_roundtrip_metrics() {
        let m = TxChannelMetrics {
            channel_id: 1,
            capacity: 256,
            queued: 10,
            active_reservations: 2,
            available: 244,
            closed: false,
        };
        let json = serde_json::to_string(&m).unwrap();
        let restored: TxChannelMetrics = serde_json::from_str(&json).unwrap();
        assert_eq!(m, restored);
    }

    #[test]
    fn serde_roundtrip_channel_metadata() {
        let m = ChannelMetadata {
            channel_id: 42,
            name: "test".into(),
            capacity: 128,
            created_at_ms: 1000,
            purpose: "testing".into(),
        };
        let json = serde_json::to_string(&m).unwrap();
        let restored: ChannelMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(m.channel_id, restored.channel_id);
        assert_eq!(m.name, restored.name);
    }

    #[test]
    fn producer_is_clone() {
        let (tx, _rx) = tx_channel::<u32>(4);
        let tx2 = tx.clone();
        tx2.try_send(42).unwrap();
        assert_eq!(tx.queued(), 1);
    }

    #[test]
    fn concurrent_reserve_rollback_stress() {
        let (tx, rx) = tx_channel::<u32>(4);

        // Reserve all 4 slots
        let r1 = tx.try_reserve().unwrap();
        let r2 = tx.try_reserve().unwrap();
        let r3 = tx.try_reserve().unwrap();
        let r4 = tx.try_reserve().unwrap();
        assert!(matches!(tx.try_reserve(), Err(TxChannelError::Full)));

        // Commit 2, rollback 2
        tx.commit(&r1, 1).unwrap();
        tx.commit(&r3, 3).unwrap();
        drop(r2); // rollback
        drop(r4); // rollback

        assert_eq!(tx.queued(), 2);
        assert_eq!(tx.active_reservations(), 0);
        assert_eq!(tx.available(), 2);

        // Consumer receives only committed values
        let v1 = rx.try_recv().unwrap();
        let v2 = rx.try_recv().unwrap();
        assert_eq!(v1.value, 1);
        assert_eq!(v2.value, 3);
        assert!(rx.try_recv().is_none());
    }
}
