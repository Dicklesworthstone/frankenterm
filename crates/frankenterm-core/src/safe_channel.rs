//! Cancellation-safe channel with reserve/commit semantics.
//!
//! Standard channel `recv()` has a silent-loss problem: if the receiving task
//! is cancelled between dequeue and processing, the message vanishes. This
//! module wraps a bounded MPMC queue with a **reserve/commit** protocol:
//!
//! 1. [`SafeReceiver::try_reserve`] / [`SafeReceiver::reserve`] dequeues the
//!    item into a staging area and returns a [`Reservation`] handle.
//! 2. The consumer inspects (`peek`) and processes the item, then calls
//!    [`Reservation::commit`] to acknowledge successful processing.
//! 3. If the [`Reservation`] is dropped without committing (e.g. the holding
//!    task is cancelled), the item is automatically returned to the **front**
//!    of the channel queue for the next consumer.
//!
//! # Integration with cancellation tokens
//!
//! The [`SafeReceiver::reserve_cancellable`] and [`SafeSender::send_cancellable`]
//! methods accept a [`CancellationToken`] and abort promptly when the token
//! fires, returning [`SafeChannelError::Cancelled`].
//!
//! # Thread safety
//!
//! - `SafeSender<T>: Send + Sync + Clone` (multiple producers)
//! - `SafeReceiver<T>: Send + Sync + Clone` (multiple consumers)
//! - `Reservation<T>: Send` (single-owner RAII guard)
//!
//! All internal state is protected by `std::sync::Mutex` + `Condvar`.

use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::cancellation::CancellationToken;

// ── Configuration ─────────────────────────────────────────────────────────

/// Configuration for a [`safe_channel`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SafeChannelConfig {
    /// Maximum items the queue can hold (must be > 0).
    pub capacity: usize,
    /// Maximum concurrent outstanding reservations.
    pub max_reservations: usize,
    /// Whether blocking methods check cancellation tokens.
    pub cancellation_aware: bool,
    /// Poll interval (ms) for cancellation checks in blocking methods.
    pub cancellation_poll_ms: u64,
}

impl Default for SafeChannelConfig {
    fn default() -> Self {
        Self {
            capacity: 64,
            max_reservations: 64,
            cancellation_aware: true,
            cancellation_poll_ms: 50,
        }
    }
}

// ── Error type ────────────────────────────────────────────────────────────

/// Errors from safe channel operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SafeChannelError {
    /// Channel has been closed; no more sends accepted.
    Closed,
    /// Channel is full; try again later.
    Full,
    /// Channel is empty; nothing to reserve.
    Empty,
    /// Too many outstanding reservations.
    ReservationLimitReached,
    /// Operation cancelled by a CancellationToken.
    Cancelled { reason: String },
    /// Reservation ID not recognized.
    InvalidReservation { id: u64 },
}

impl fmt::Display for SafeChannelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => write!(f, "channel closed"),
            Self::Full => write!(f, "channel full"),
            Self::Empty => write!(f, "channel empty"),
            Self::ReservationLimitReached => write!(f, "reservation limit reached"),
            Self::Cancelled { reason } => write!(f, "cancelled: {reason}"),
            Self::InvalidReservation { id } => write!(f, "invalid reservation: {id}"),
        }
    }
}

impl std::error::Error for SafeChannelError {}

// ── Reservation ID ────────────────────────────────────────────────────────

/// Opaque identifier for a reserved item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ReservationId(pub u64);

impl fmt::Display for ReservationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "res-{}", self.0)
    }
}

// ── Metrics ───────────────────────────────────────────────────────────────

/// Observable metrics for a safe channel instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SafeChannelMetrics {
    /// Total items sent successfully.
    pub total_sent: u64,
    /// Total items committed (fully processed).
    pub total_committed: u64,
    /// Total explicit rollbacks via `Reservation::rollback`.
    pub total_rollbacks: u64,
    /// Total drop-triggered rollbacks (cancellation safety activations).
    pub total_drop_rollbacks: u64,
    /// Total items currently in queue.
    pub queue_depth: usize,
    /// Total items currently reserved (in-flight).
    pub reserved_count: usize,
    /// Total failed sends (Full or Closed).
    pub total_send_failures: u64,
    /// High-water mark for queue depth.
    pub queue_hwm: usize,
    /// High-water mark for simultaneous reservations.
    pub reservation_hwm: usize,
}

// ── Internal shared state ─────────────────────────────────────────────────

struct ChannelState<T> {
    queue: VecDeque<T>,
    /// Number of currently active reservations (items in-flight).
    active_reservations: usize,
    closed: bool,
    // Metrics counters
    total_sent: u64,
    total_committed: u64,
    total_rollbacks: u64,
    total_drop_rollbacks: u64,
    total_send_failures: u64,
    queue_hwm: usize,
    reservation_hwm: usize,
}

impl<T> ChannelState<T> {
    fn new(capacity: usize) -> Self {
        Self {
            queue: VecDeque::with_capacity(capacity),
            active_reservations: 0,
            closed: false,
            total_sent: 0,
            total_committed: 0,
            total_rollbacks: 0,
            total_drop_rollbacks: 0,
            total_send_failures: 0,
            queue_hwm: 0,
            reservation_hwm: 0,
        }
    }

    fn snapshot_metrics(&self) -> SafeChannelMetrics {
        SafeChannelMetrics {
            total_sent: self.total_sent,
            total_committed: self.total_committed,
            total_rollbacks: self.total_rollbacks,
            total_drop_rollbacks: self.total_drop_rollbacks,
            queue_depth: self.queue.len(),
            reserved_count: self.active_reservations,
            total_send_failures: self.total_send_failures,
            queue_hwm: self.queue_hwm,
            reservation_hwm: self.reservation_hwm,
        }
    }

    fn track_queue_hwm(&mut self) {
        let depth = self.queue.len();
        if depth > self.queue_hwm {
            self.queue_hwm = depth;
        }
    }

    fn track_reservation_hwm(&mut self) {
        if self.active_reservations > self.reservation_hwm {
            self.reservation_hwm = self.active_reservations;
        }
    }
}

struct SafeChannelShared<T> {
    state: Mutex<ChannelState<T>>,
    not_empty: Condvar,
    not_full: Condvar,
    reservation_released: Condvar,
    config: SafeChannelConfig,
    /// Global reservation counter (atomic for ID generation without lock).
    global_res_counter: AtomicU64,
}

// ── Constructor ───────────────────────────────────────────────────────────

/// Create a cancellation-safe channel pair.
///
/// # Panics
///
/// Panics if `config.capacity == 0`.
pub fn safe_channel<T: Send>(config: SafeChannelConfig) -> (SafeSender<T>, SafeReceiver<T>) {
    assert!(config.capacity > 0, "channel capacity must be > 0");
    let shared = Arc::new(SafeChannelShared {
        state: Mutex::new(ChannelState::new(config.capacity)),
        not_empty: Condvar::new(),
        not_full: Condvar::new(),
        reservation_released: Condvar::new(),
        config,
        global_res_counter: AtomicU64::new(0),
    });
    (
        SafeSender {
            shared: Arc::clone(&shared),
        },
        SafeReceiver {
            shared: Arc::clone(&shared),
        },
    )
}

// ── Sender ────────────────────────────────────────────────────────────────

/// Sending half of a cancellation-safe channel.
pub struct SafeSender<T> {
    shared: Arc<SafeChannelShared<T>>,
}

impl<T> Clone for SafeSender<T> {
    fn clone(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<T: Send> SafeSender<T> {
    /// Non-blocking send. Returns `Full` if at capacity, `Closed` if shut down.
    pub fn try_send(&self, item: T) -> Result<(), SafeChannelError> {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed {
            state.total_send_failures += 1;
            return Err(SafeChannelError::Closed);
        }
        if state.queue.len() >= self.shared.config.capacity {
            state.total_send_failures += 1;
            return Err(SafeChannelError::Full);
        }
        state.queue.push_back(item);
        state.total_sent += 1;
        state.track_queue_hwm();
        self.shared.not_empty.notify_one();
        Ok(())
    }

    /// Blocking send. Waits until space is available or channel is closed.
    pub fn send(&self, item: T) -> Result<(), SafeChannelError> {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if state.closed {
                state.total_send_failures += 1;
                return Err(SafeChannelError::Closed);
            }
            if state.queue.len() < self.shared.config.capacity {
                state.queue.push_back(item);
                state.total_sent += 1;
                state.track_queue_hwm();
                self.shared.not_empty.notify_one();
                return Ok(());
            }
            state = self
                .shared
                .not_full
                .wait(state)
                .unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Blocking send with cancellation awareness.
    pub fn send_cancellable(
        &self,
        item: T,
        token: &CancellationToken,
    ) -> Result<(), SafeChannelError> {
        let poll = Duration::from_millis(self.shared.config.cancellation_poll_ms);
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if state.closed {
                state.total_send_failures += 1;
                return Err(SafeChannelError::Closed);
            }
            if token.is_cancelled() {
                return Err(SafeChannelError::Cancelled {
                    reason: token
                        .reason()
                        .map(|r| r.to_string())
                        .unwrap_or_else(|| "unknown".into()),
                });
            }
            if state.queue.len() < self.shared.config.capacity {
                state.queue.push_back(item);
                state.total_sent += 1;
                state.track_queue_hwm();
                self.shared.not_empty.notify_one();
                return Ok(());
            }
            let result = self
                .shared
                .not_full
                .wait_timeout(state, poll)
                .unwrap_or_else(|e| e.into_inner());
            state = result.0;
        }
    }

    /// Close the sending side. Pending items can still be reserved/committed.
    pub fn close(&self) {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.closed = true;
        // Wake all waiters so they see the closed state
        self.shared.not_empty.notify_all();
        self.shared.not_full.notify_all();
    }

    /// Current queue length (excludes reserved items).
    pub fn len(&self) -> usize {
        let state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.queue.len()
    }

    /// Whether the queue is empty (excludes reserved items).
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether the channel is closed.
    pub fn is_closed(&self) -> bool {
        let state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.closed
    }

    /// Snapshot current metrics.
    pub fn metrics(&self) -> SafeChannelMetrics {
        let state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.snapshot_metrics()
    }
}

// ── Receiver ──────────────────────────────────────────────────────────────

/// Receiving half of a cancellation-safe channel.
pub struct SafeReceiver<T> {
    shared: Arc<SafeChannelShared<T>>,
}

impl<T> Clone for SafeReceiver<T> {
    fn clone(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<T: Send> SafeReceiver<T> {
    /// Non-blocking reserve. Dequeues from front into staging, returns a
    /// [`Reservation`] handle. Returns `Empty` if nothing available, or
    /// `ReservationLimitReached` if too many items are in-flight.
    pub fn try_reserve(&self) -> Result<Reservation<T>, SafeChannelError> {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.active_reservations >= self.shared.config.max_reservations {
            return Err(SafeChannelError::ReservationLimitReached);
        }
        match state.queue.pop_front() {
            Some(item) => {
                let rid = self.shared.global_res_counter.fetch_add(1, Ordering::Relaxed);
                state.active_reservations += 1;
                state.track_reservation_hwm();
                self.shared.not_full.notify_one();
                Ok(Reservation {
                    item: Some(item),
                    id: ReservationId(rid),
                    shared: Arc::clone(&self.shared),
                    resolved: false,
                })
            }
            None => {
                if state.closed {
                    Err(SafeChannelError::Closed)
                } else {
                    Err(SafeChannelError::Empty)
                }
            }
        }
    }

    /// Blocking reserve. Waits until an item is available.
    pub fn reserve(&self) -> Result<Reservation<T>, SafeChannelError> {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if state.active_reservations < self.shared.config.max_reservations {
                if let Some(item) = state.queue.pop_front() {
                    let rid =
                        self.shared.global_res_counter.fetch_add(1, Ordering::Relaxed);
                    state.active_reservations += 1;
                    state.track_reservation_hwm();
                    self.shared.not_full.notify_one();
                    return Ok(Reservation {
                        item: Some(item),
                        id: ReservationId(rid),
                        shared: Arc::clone(&self.shared),
                        resolved: false,
                    });
                }
            }
            if state.closed && state.queue.is_empty() {
                return Err(SafeChannelError::Closed);
            }
            state = self
                .shared
                .not_empty
                .wait(state)
                .unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Blocking reserve with cancellation awareness.
    pub fn reserve_cancellable(
        &self,
        token: &CancellationToken,
    ) -> Result<Reservation<T>, SafeChannelError> {
        let poll = Duration::from_millis(self.shared.config.cancellation_poll_ms);
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if state.active_reservations < self.shared.config.max_reservations {
                if let Some(item) = state.queue.pop_front() {
                    let rid =
                        self.shared.global_res_counter.fetch_add(1, Ordering::Relaxed);
                    state.active_reservations += 1;
                    state.track_reservation_hwm();
                    self.shared.not_full.notify_one();
                    return Ok(Reservation {
                        item: Some(item),
                        id: ReservationId(rid),
                        shared: Arc::clone(&self.shared),
                        resolved: false,
                    });
                }
            }
            if state.closed && state.queue.is_empty() {
                return Err(SafeChannelError::Closed);
            }
            if token.is_cancelled() {
                return Err(SafeChannelError::Cancelled {
                    reason: token
                        .reason()
                        .map(|r| r.to_string())
                        .unwrap_or_else(|| "unknown".into()),
                });
            }
            let result = self
                .shared
                .not_empty
                .wait_timeout(state, poll)
                .unwrap_or_else(|e| e.into_inner());
            state = result.0;
        }
    }

    /// Non-blocking immediate receive (reserve + auto-commit).
    pub fn try_recv(&self) -> Result<T, SafeChannelError> {
        let reservation = self.try_reserve()?;
        Ok(reservation.commit())
    }

    /// Number of outstanding reservations.
    pub fn pending_reservations(&self) -> usize {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .active_reservations
    }

    /// Whether the queue and reservations are both empty.
    pub fn is_drained(&self) -> bool {
        let state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.queue.is_empty() && state.reservations.is_empty()
    }

    /// Snapshot current metrics.
    pub fn metrics(&self) -> SafeChannelMetrics {
        let state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.snapshot_metrics()
    }
}

// ── Reservation (RAII guard) ──────────────────────────────────────────────

/// A reserved item from a cancellation-safe channel.
///
/// The holder must call [`commit`](Reservation::commit) to acknowledge
/// successful processing. If this guard is dropped without committing,
/// the item is automatically returned to the front of the channel queue.
pub struct Reservation<T> {
    item: Option<T>,
    id: ReservationId,
    shared: Arc<SafeChannelShared<T>>,
    resolved: bool,
}

impl<T> fmt::Debug for Reservation<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Reservation")
            .field("id", &self.id)
            .field("resolved", &self.resolved)
            .field("has_item", &self.item.is_some())
            .finish()
    }
}

impl<T: Send> Reservation<T> {
    /// The reservation identifier.
    pub fn id(&self) -> ReservationId {
        self.id
    }

    /// Borrow the reserved item without consuming.
    pub fn peek(&self) -> &T {
        self.item.as_ref().expect("reservation already resolved")
    }

    /// Acknowledge successful processing. Consumes the reservation and
    /// returns the item.
    pub fn commit(mut self) -> T {
        self.resolved = true;
        let item = self.item.take().expect("reservation already resolved");
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        state.total_committed += 1;
        state.active_reservations = state.active_reservations.saturating_sub(1);
        drop(state);
        self.shared.not_full.notify_one();
        self.shared.reservation_released.notify_one();
        item
    }

    /// Explicitly return the item to the front of the queue.
    pub fn rollback(mut self) {
        self.resolved = true;
        let item = self.item.take().expect("reservation already resolved");
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        state.queue.push_front(item);
        state.total_rollbacks += 1;
        state.active_reservations = state.active_reservations.saturating_sub(1);
        state.track_queue_hwm();
        drop(state);
        self.shared.not_empty.notify_one();
        self.shared.reservation_released.notify_one();
    }

    /// Alias for [`commit`](Reservation::commit).
    pub fn into_inner(self) -> T {
        self.commit()
    }
}

impl<T: Send> Drop for Reservation<T> {
    fn drop(&mut self) {
        if !self.resolved {
            if let Some(item) = self.item.take() {
                // Automatic rollback — the cancellation-safety guarantee.
                // Use unwrap_or_else to handle mutex poisoning gracefully
                // (we must not panic in a destructor).
                let mut state = self
                    .shared
                    .state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                state.queue.push_front(item);
                state.total_drop_rollbacks += 1;
                state.active_reservations = state.active_reservations.saturating_sub(1);
                state.track_queue_hwm();
                drop(state);
                self.shared.not_empty.notify_one();
                self.shared.reservation_released.notify_one();
            }
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cancellation::ShutdownReason;
    use crate::scope_tree::ScopeId;
    use std::thread;

    fn default_config() -> SafeChannelConfig {
        SafeChannelConfig {
            capacity: 8,
            max_reservations: 8,
            cancellation_aware: true,
            cancellation_poll_ms: 10,
        }
    }

    fn small_config(cap: usize) -> SafeChannelConfig {
        SafeChannelConfig {
            capacity: cap,
            max_reservations: cap,
            cancellation_aware: true,
            cancellation_poll_ms: 10,
        }
    }

    #[test]
    fn basic_send_recv() {
        let (tx, rx) = safe_channel::<u32>(default_config());
        tx.try_send(42).unwrap();
        let val = rx.try_recv().unwrap();
        assert_eq!(val, 42);
    }

    #[test]
    fn reserve_commit_flow() {
        let (tx, rx) = safe_channel::<String>(default_config());
        tx.try_send("hello".into()).unwrap();

        let res = rx.try_reserve().unwrap();
        assert_eq!(res.peek(), "hello");
        let val = res.commit();
        assert_eq!(val, "hello");

        let m = rx.metrics();
        assert_eq!(m.total_committed, 1);
        assert_eq!(m.total_sent, 1);
    }

    #[test]
    fn reserve_rollback_explicit() {
        let (tx, rx) = safe_channel::<u32>(default_config());
        tx.try_send(99).unwrap();

        let res = rx.try_reserve().unwrap();
        assert_eq!(*res.peek(), 99);
        res.rollback();

        // Item should be back in queue
        let val = rx.try_recv().unwrap();
        assert_eq!(val, 99);

        let m = rx.metrics();
        assert_eq!(m.total_rollbacks, 1);
    }

    #[test]
    fn reserve_drop_rollback() {
        let (tx, rx) = safe_channel::<u32>(default_config());
        tx.try_send(77).unwrap();

        {
            let _res = rx.try_reserve().unwrap();
            // Drop without commit — simulates cancellation
        }

        // Item should be back in queue
        let val = rx.try_recv().unwrap();
        assert_eq!(val, 77);

        let m = rx.metrics();
        assert_eq!(m.total_drop_rollbacks, 1);
    }

    #[test]
    fn fifo_ordering() {
        let (tx, rx) = safe_channel::<u32>(default_config());
        for i in 0..5 {
            tx.try_send(i).unwrap();
        }
        for i in 0..5 {
            let val = rx.try_recv().unwrap();
            assert_eq!(val, i);
        }
    }

    #[test]
    fn rollback_goes_to_front() {
        let (tx, rx) = safe_channel::<u32>(default_config());
        tx.try_send(1).unwrap();
        tx.try_send(2).unwrap();
        tx.try_send(3).unwrap();

        let r1 = rx.try_reserve().unwrap(); // gets 1
        let _r2 = rx.try_reserve().unwrap(); // gets 2

        // Rollback r1 — should go to front
        r1.rollback();

        // Next reserve should get 1 (from front), not 3
        let r3 = rx.try_reserve().unwrap();
        assert_eq!(*r3.peek(), 1);
        r3.commit();
    }

    #[test]
    fn channel_full_behavior() {
        let (tx, _rx) = safe_channel::<u32>(small_config(2));
        tx.try_send(1).unwrap();
        tx.try_send(2).unwrap();
        assert_eq!(tx.try_send(3), Err(SafeChannelError::Full));

        let m = tx.metrics();
        assert_eq!(m.total_send_failures, 1);
    }

    #[test]
    fn channel_empty_behavior() {
        let (_tx, rx) = safe_channel::<u32>(default_config());
        assert_eq!(rx.try_reserve().err(), Some(SafeChannelError::Empty));
    }

    #[test]
    fn channel_close_sender() {
        let (tx, rx) = safe_channel::<u32>(default_config());
        tx.try_send(1).unwrap();
        tx.try_send(2).unwrap();
        tx.close();

        // Can still drain existing items
        assert_eq!(rx.try_recv().unwrap(), 1);
        assert_eq!(rx.try_recv().unwrap(), 2);

        // Now closed and empty
        assert_eq!(rx.try_reserve().err(), Some(SafeChannelError::Closed));
    }

    #[test]
    fn reservation_limit() {
        let (tx, rx) = safe_channel::<u32>(SafeChannelConfig {
            capacity: 8,
            max_reservations: 2,
            cancellation_aware: true,
            cancellation_poll_ms: 10,
        });
        tx.try_send(1).unwrap();
        tx.try_send(2).unwrap();
        tx.try_send(3).unwrap();

        let _r1 = rx.try_reserve().unwrap();
        let _r2 = rx.try_reserve().unwrap();
        // Third should fail — max_reservations is 2
        assert_eq!(
            rx.try_reserve().err(),
            Some(SafeChannelError::ReservationLimitReached)
        );

        // Committing one should allow a new reservation
        _r1.commit();
        let _r3 = rx.try_reserve().unwrap();
        assert_eq!(*_r3.peek(), 3);
    }

    #[test]
    fn blocking_send_wakes_on_commit() {
        let (tx, rx) = safe_channel::<u32>(small_config(1));
        tx.try_send(1).unwrap();

        let tx2 = tx.clone();
        let handle = thread::spawn(move || {
            tx2.send(2).unwrap(); // blocks until space
        });

        // Reserve and commit to free space
        let res = rx.try_reserve().unwrap();
        res.commit();

        handle.join().unwrap();
        assert_eq!(rx.try_recv().unwrap(), 2);
    }

    #[test]
    fn blocking_reserve_wakes_on_send() {
        let (tx, rx) = safe_channel::<u32>(default_config());

        let rx2 = rx.clone();
        let handle = thread::spawn(move || {
            let res = rx2.reserve().unwrap(); // blocks until item
            res.commit()
        });

        thread::sleep(Duration::from_millis(20));
        tx.try_send(42).unwrap();

        let val = handle.join().unwrap();
        assert_eq!(val, 42);
    }

    #[test]
    fn cancellable_reserve_cancelled() {
        let (_tx, rx) = safe_channel::<u32>(default_config());
        let token = CancellationToken::new(ScopeId("test-cancel".into()));

        let rx2 = rx.clone();
        let token2 = token.child(ScopeId("test-cancel-child".into()));
        let handle = thread::spawn(move || rx2.reserve_cancellable(&token2));

        thread::sleep(Duration::from_millis(30));
        token.cancel(ShutdownReason::UserRequested);

        let result = handle.join().unwrap();
        assert!(matches!(result, Err(SafeChannelError::Cancelled { .. })));
    }

    #[test]
    fn cancellable_send_cancelled() {
        let (tx, _rx) = safe_channel::<u32>(small_config(1));
        tx.try_send(1).unwrap(); // fill it

        let token = CancellationToken::new(ScopeId("test-cancel-send".into()));
        let tx2 = tx.clone();
        let token2 = token.child(ScopeId("test-cancel-send-child".into()));
        let handle = thread::spawn(move || tx2.send_cancellable(2, &token2));

        thread::sleep(Duration::from_millis(30));
        token.cancel(ShutdownReason::UserRequested);

        let result = handle.join().unwrap();
        assert!(matches!(result, Err(SafeChannelError::Cancelled { .. })));
    }

    #[test]
    fn multiple_consumers_no_duplicates() {
        let (tx, rx) = safe_channel::<u32>(default_config());
        for i in 0..100 {
            tx.try_send(i).unwrap();
        }
        tx.close();

        let rx1 = rx.clone();
        let rx2 = rx.clone();

        let h1 = thread::spawn(move || {
            let mut items = Vec::new();
            loop {
                match rx1.try_reserve() {
                    Ok(res) => items.push(res.commit()),
                    Err(SafeChannelError::Empty) => {
                        thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    Err(SafeChannelError::Closed) => break,
                    Err(e) => panic!("unexpected: {e}"),
                }
            }
            items
        });

        let h2 = thread::spawn(move || {
            let mut items = Vec::new();
            loop {
                match rx2.try_reserve() {
                    Ok(res) => items.push(res.commit()),
                    Err(SafeChannelError::Empty) => {
                        thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    Err(SafeChannelError::Closed) => break,
                    Err(e) => panic!("unexpected: {e}"),
                }
            }
            items
        });

        let mut all: Vec<u32> = Vec::new();
        all.extend(h1.join().unwrap());
        all.extend(h2.join().unwrap());
        all.sort();
        let expected: Vec<u32> = (0..100).collect();
        assert_eq!(all, expected);
    }

    #[test]
    fn multiple_producers() {
        let (tx, rx) = safe_channel::<u32>(SafeChannelConfig {
            capacity: 200,
            max_reservations: 200,
            cancellation_aware: true,
            cancellation_poll_ms: 10,
        });

        let tx1 = tx.clone();
        let tx2 = tx.clone();

        let h1 = thread::spawn(move || {
            for i in 0..50 {
                tx1.try_send(i).unwrap();
            }
        });
        let h2 = thread::spawn(move || {
            for i in 50..100 {
                tx2.try_send(i).unwrap();
            }
        });

        h1.join().unwrap();
        h2.join().unwrap();
        tx.close();

        let mut items = Vec::new();
        loop {
            match rx.try_recv() {
                Ok(v) => items.push(v),
                Err(SafeChannelError::Empty) | Err(SafeChannelError::Closed) => break,
                Err(e) => panic!("unexpected: {e}"),
            }
        }
        items.sort();
        let expected: Vec<u32> = (0..100).collect();
        assert_eq!(items, expected);
    }

    #[test]
    fn metrics_tracking() {
        let (tx, rx) = safe_channel::<u32>(default_config());
        tx.try_send(1).unwrap();
        tx.try_send(2).unwrap();
        tx.try_send(3).unwrap();

        let m = tx.metrics();
        assert_eq!(m.total_sent, 3);
        assert_eq!(m.queue_depth, 3);

        // Reserve and commit one
        let res = rx.try_reserve().unwrap();
        res.commit();

        // Reserve and rollback one
        let res = rx.try_reserve().unwrap();
        res.rollback();

        // Reserve and drop one
        {
            let _res = rx.try_reserve().unwrap();
        }

        let m = rx.metrics();
        assert_eq!(m.total_committed, 1);
        assert_eq!(m.total_rollbacks, 1);
        assert_eq!(m.total_drop_rollbacks, 1);
        assert_eq!(m.queue_depth, 2); // rollback + drop-rollback re-enqueued 2
    }

    #[test]
    fn metrics_hwm() {
        let (tx, rx) = safe_channel::<u32>(default_config());
        for i in 0..5 {
            tx.try_send(i).unwrap();
        }
        let m = tx.metrics();
        assert_eq!(m.queue_hwm, 5);

        // Drain all
        for _ in 0..5 {
            rx.try_recv().unwrap();
        }
        let m = rx.metrics();
        assert_eq!(m.queue_hwm, 5); // HWM doesn't decrease
        assert_eq!(m.queue_depth, 0);
    }

    #[test]
    fn commit_returns_item() {
        let (tx, rx) = safe_channel::<String>(default_config());
        tx.try_send("value".into()).unwrap();
        let res = rx.try_reserve().unwrap();
        let val = res.commit();
        assert_eq!(val, "value");
    }

    #[test]
    fn peek_does_not_consume() {
        let (tx, rx) = safe_channel::<u32>(default_config());
        tx.try_send(42).unwrap();
        let res = rx.try_reserve().unwrap();
        assert_eq!(*res.peek(), 42);
        assert_eq!(*res.peek(), 42); // peek again
        let val = res.commit();
        assert_eq!(val, 42);
    }

    #[test]
    fn serde_roundtrip_config() {
        let config = SafeChannelConfig {
            capacity: 128,
            max_reservations: 32,
            cancellation_aware: false,
            cancellation_poll_ms: 100,
        };
        let json = serde_json::to_string(&config).unwrap();
        let decoded: SafeChannelConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.capacity, 128);
        assert_eq!(decoded.max_reservations, 32);
        assert!(!decoded.cancellation_aware);
        assert_eq!(decoded.cancellation_poll_ms, 100);
    }

    #[test]
    fn serde_roundtrip_metrics() {
        let m = SafeChannelMetrics {
            total_sent: 100,
            total_committed: 90,
            total_rollbacks: 5,
            total_drop_rollbacks: 3,
            queue_depth: 2,
            reserved_count: 0,
            total_send_failures: 1,
            queue_hwm: 50,
            reservation_hwm: 10,
        };
        let json = serde_json::to_string(&m).unwrap();
        let decoded: SafeChannelMetrics = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, m);
    }

    #[test]
    fn close_wakes_blocking_reserve() {
        let (tx, rx) = safe_channel::<u32>(default_config());

        let rx2 = rx.clone();
        let handle = thread::spawn(move || rx2.reserve());

        thread::sleep(Duration::from_millis(30));
        tx.close();

        let result = handle.join().unwrap();
        assert_eq!(result, Err(SafeChannelError::Closed));
    }

    #[test]
    fn config_default_values() {
        let config = SafeChannelConfig::default();
        assert_eq!(config.capacity, 64);
        assert_eq!(config.max_reservations, 64);
        assert!(config.cancellation_aware);
        assert_eq!(config.cancellation_poll_ms, 50);
    }

    // ── Stress tests ──────────────────────────────────────────────────────

    #[test]
    fn concurrent_reserve_commit_no_loss() {
        let total_items = 1000;
        let num_producers = 4;
        let num_consumers = 4;
        let items_per_producer = total_items / num_producers;

        let (tx, rx) = safe_channel::<u32>(SafeChannelConfig {
            capacity: 256,
            max_reservations: 256,
            cancellation_aware: true,
            cancellation_poll_ms: 10,
        });

        // Producers
        let mut producers = Vec::new();
        for p in 0..num_producers {
            let tx = tx.clone();
            let start = (p * items_per_producer) as u32;
            let end = start + items_per_producer as u32;
            producers.push(thread::spawn(move || {
                for i in start..end {
                    tx.send(i).unwrap();
                }
            }));
        }

        // Close after all producers done
        let tx_close = tx.clone();
        let closer = thread::spawn(move || {
            for p in producers {
                p.join().unwrap();
            }
            tx_close.close();
        });

        // Consumers
        let committed = Arc::new(Mutex::new(Vec::new()));
        let mut consumers = Vec::new();
        for _ in 0..num_consumers {
            let rx = rx.clone();
            let committed = Arc::clone(&committed);
            consumers.push(thread::spawn(move || {
                loop {
                    match rx.reserve() {
                        Ok(res) => {
                            let val = res.commit();
                            committed.lock().unwrap().push(val);
                        }
                        Err(SafeChannelError::Closed) => break,
                        Err(e) => panic!("unexpected: {e}"),
                    }
                }
            }));
        }

        closer.join().unwrap();
        for c in consumers {
            c.join().unwrap();
        }

        let mut all = committed.lock().unwrap().clone();
        all.sort();
        let expected: Vec<u32> = (0..total_items as u32).collect();
        assert_eq!(all, expected, "no items lost");
    }

    #[test]
    fn concurrent_reserve_drop_no_loss() {
        let total_items = 200;
        let (tx, rx) = safe_channel::<u32>(SafeChannelConfig {
            capacity: 256,
            max_reservations: 256,
            cancellation_aware: true,
            cancellation_poll_ms: 10,
        });

        for i in 0..total_items {
            tx.try_send(i).unwrap();
        }
        tx.close();

        let committed = Arc::new(Mutex::new(Vec::new()));
        let drop_count = Arc::new(AtomicU64::new(0));
        let mut consumers = Vec::new();

        for t in 0..4u32 {
            let rx = rx.clone();
            let committed = Arc::clone(&committed);
            let drop_count = Arc::clone(&drop_count);
            consumers.push(thread::spawn(move || {
                loop {
                    match rx.reserve() {
                        Ok(res) => {
                            // Every 3rd item from thread 0 and 1: drop (rollback)
                            if t < 2 && *res.peek() % 3 == 0 {
                                drop_count.fetch_add(1, Ordering::Relaxed);
                                drop(res); // auto-rollback
                            } else {
                                let val = res.commit();
                                committed.lock().unwrap().push(val);
                            }
                        }
                        Err(SafeChannelError::Closed) => break,
                        Err(e) => panic!("unexpected: {e}"),
                    }
                }
            }));
        }

        for c in consumers {
            c.join().unwrap();
        }

        let mut all = committed.lock().unwrap().clone();
        all.sort();
        all.dedup();
        let expected: Vec<u32> = (0..total_items).collect();
        assert_eq!(all, expected, "all items eventually committed");
    }

    #[test]
    fn stress_alternating_send_cancel() {
        let (tx, rx) = safe_channel::<u32>(small_config(16));
        let token = CancellationToken::new(ScopeId("stress-root".into()));

        // Producer: sends items as fast as possible
        let tx2 = tx.clone();
        let producer = thread::spawn(move || {
            for i in 0..500u32 {
                match tx2.try_send(i) {
                    Ok(()) => {}
                    Err(SafeChannelError::Full) => thread::yield_now(),
                    Err(SafeChannelError::Closed) => break,
                    Err(e) => panic!("unexpected: {e}"),
                }
            }
        });

        // Consumer with cancellation
        let rx2 = rx.clone();
        let child_token = token.child(ScopeId("stress-consumer".into()));
        let consumer = thread::spawn(move || {
            let mut committed = 0u32;
            loop {
                match rx2.reserve_cancellable(&child_token) {
                    Ok(res) => {
                        res.commit();
                        committed += 1;
                    }
                    Err(SafeChannelError::Cancelled { .. }) => break,
                    Err(SafeChannelError::Empty) => thread::yield_now(),
                    Err(SafeChannelError::Closed) => break,
                    Err(e) => panic!("unexpected: {e}"),
                }
            }
            committed
        });

        // Let it run briefly then cancel
        thread::sleep(Duration::from_millis(50));
        token.cancel(ShutdownReason::UserRequested);
        tx.close();

        producer.join().unwrap();
        let committed = consumer.join().unwrap();
        // Just verify no panics/deadlocks — committed count varies
        assert!(committed > 0, "some items should have been committed");
    }
}
