//! Lock-free ring buffer channels for hot-path capture fanout.
//!
//! This module provides:
//! - `channel`: bounded single-producer/single-consumer (SPSC)
//! - `spmc_channel`: bounded single-producer/multi-consumer (SPMC) broadcast
//!
//! Internally it uses
//! `crossbeam::queue::ArrayQueue`, which provides lock-free bounded queue
//! operations without requiring unsafe code in this crate.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::runtime_compat::notify::Notify;
use crossbeam::queue::ArrayQueue;

/// Construct a bounded SPSC channel.
///
/// # Panics
/// Panics when `capacity == 0`.
pub fn channel<T>(capacity: usize) -> (SpscProducer<T>, SpscConsumer<T>) {
    assert!(capacity > 0, "SPSC capacity must be > 0");
    let shared = Arc::new(Shared::new(capacity));
    (
        SpscProducer {
            shared: Arc::clone(&shared),
        },
        SpscConsumer { shared },
    )
}

struct Shared<T> {
    queue: ArrayQueue<T>,
    closed: AtomicBool,
    not_empty: Notify,
    not_full: Notify,
}

/// Construct a bounded SPMC broadcast channel.
///
/// The producer is single-threaded; each consumer gets its own lock-free queue.
/// A send succeeds only when *all* consumer queues can accept the value.
///
/// # Panics
/// Panics when `capacity == 0` or `consumers == 0`.
pub fn spmc_channel<T: Clone>(
    capacity: usize,
    consumers: usize,
) -> (SpmcProducer<T>, Vec<SpmcConsumer<T>>) {
    assert!(capacity > 0, "SPMC capacity must be > 0");
    assert!(consumers > 0, "SPMC consumers must be > 0");

    let shared = Arc::new(SpmcShared::new(capacity, consumers));
    let mut receivers = Vec::with_capacity(consumers);
    for consumer_idx in 0..consumers {
        receivers.push(SpmcConsumer {
            shared: Arc::clone(&shared),
            consumer_idx,
        });
    }

    (SpmcProducer { shared }, receivers)
}

struct SpmcShared<T> {
    queues: Vec<ArrayQueue<T>>,
    closed: AtomicBool,
    not_empty: Vec<Notify>,
    not_full: Vec<Notify>,
}

impl<T> SpmcShared<T> {
    fn new(capacity: usize, consumers: usize) -> Self {
        let mut queues = Vec::with_capacity(consumers);
        let mut not_empty = Vec::with_capacity(consumers);
        let mut not_full = Vec::with_capacity(consumers);
        for _ in 0..consumers {
            queues.push(ArrayQueue::new(capacity));
            not_empty.push(Notify::new());
            not_full.push(Notify::new());
        }
        Self {
            queues,
            closed: AtomicBool::new(false),
            not_empty,
            not_full,
        }
    }
}

impl<T> Shared<T> {
    fn new(capacity: usize) -> Self {
        Self {
            queue: ArrayQueue::new(capacity),
            closed: AtomicBool::new(false),
            not_empty: Notify::new(),
            not_full: Notify::new(),
        }
    }
}

/// Producer side of the SPSC channel.
pub struct SpscProducer<T> {
    shared: Arc<Shared<T>>,
}

impl<T> SpscProducer<T> {
    /// Send a value, waiting asynchronously when the ring is full.
    pub async fn send(&self, mut value: T) -> Result<(), T> {
        loop {
            if self.is_closed() {
                return Err(value);
            }

            match self.shared.queue.push(value) {
                Ok(()) => {
                    self.shared.not_empty.notify_one();
                    return Ok(());
                }
                Err(v) => {
                    value = v;
                    let notified = self.shared.not_full.notified();
                    if self.shared.queue.is_full() && !self.is_closed() {
                        notified.await;
                    }
                }
            }
        }
    }

    /// Try to send a value without waiting.
    pub fn try_send(&self, value: T) -> Result<(), T> {
        if self.is_closed() {
            return Err(value);
        }

        match self.shared.queue.push(value) {
            Ok(()) => {
                self.shared.not_empty.notify_one();
                Ok(())
            }
            Err(v) => Err(v),
        }
    }

    /// Mark this channel as closed.
    pub fn close(&self) {
        if !self.shared.closed.swap(true, Ordering::AcqRel) {
            self.shared.not_empty.notify_waiters();
            self.shared.not_full.notify_waiters();
        }
    }

    /// Returns true if the channel is closed.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::Acquire)
    }

    /// Current queue depth.
    #[must_use]
    pub fn depth(&self) -> usize {
        self.shared.queue.len()
    }

    /// Queue capacity.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.shared.queue.capacity()
    }
}

impl<T> Drop for SpscProducer<T> {
    fn drop(&mut self) {
        self.close();
    }
}

/// Consumer side of the SPSC channel.
pub struct SpscConsumer<T> {
    shared: Arc<Shared<T>>,
}

impl<T> SpscConsumer<T> {
    /// Receive one value, waiting asynchronously until data is available.
    ///
    /// Returns `None` once the channel is closed and fully drained.
    pub async fn recv(&self) -> Option<T> {
        loop {
            if let Some(value) = self.try_recv() {
                return Some(value);
            }

            if self.is_closed() {
                return None;
            }

            let notified = self.shared.not_empty.notified();
            if self.shared.queue.is_empty() && !self.is_closed() {
                notified.await;
            }
        }
    }

    /// Try to receive one value without waiting.
    pub fn try_recv(&self) -> Option<T> {
        let value = self.shared.queue.pop();
        if value.is_some() {
            self.shared.not_full.notify_one();
        }
        value
    }

    /// Returns true if the channel is closed.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::Acquire)
    }

    /// Current queue depth.
    #[must_use]
    pub fn depth(&self) -> usize {
        self.shared.queue.len()
    }

    /// Queue capacity.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.shared.queue.capacity()
    }
}

/// Producer side of the SPMC broadcast channel.
pub struct SpmcProducer<T> {
    shared: Arc<SpmcShared<T>>,
}

impl<T: Clone> SpmcProducer<T> {
    /// Send a value to all consumers, waiting asynchronously if any consumer queue is full.
    pub async fn send(&self, value: T) -> Result<(), T> {
        loop {
            if self.is_closed() {
                return Err(value);
            }

            if let Some(full_idx) = self.first_full_queue() {
                let notified = self.shared.not_full[full_idx].notified();
                if self.shared.queues[full_idx].is_full() && !self.is_closed() {
                    notified.await;
                }
                continue;
            }

            self.push_to_all(value);
            return Ok(());
        }
    }

    /// Try to send a value to all consumers without waiting.
    pub fn try_send(&self, value: T) -> Result<(), T> {
        if self.is_closed() || self.first_full_queue().is_some() {
            return Err(value);
        }
        self.push_to_all(value);
        Ok(())
    }

    /// Mark this channel as closed.
    pub fn close(&self) {
        if !self.shared.closed.swap(true, Ordering::AcqRel) {
            for notify in &self.shared.not_empty {
                notify.notify_waiters();
            }
            for notify in &self.shared.not_full {
                notify.notify_waiters();
            }
        }
    }

    /// Returns true if the channel is closed.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::Acquire)
    }

    /// Returns the maximum queue depth across consumers.
    #[must_use]
    pub fn max_depth(&self) -> usize {
        self.shared
            .queues
            .iter()
            .map(ArrayQueue::len)
            .max()
            .unwrap_or(0)
    }

    /// Queue capacity per consumer.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.shared.queues.first().map_or(0, ArrayQueue::capacity)
    }

    /// Number of registered consumers.
    #[must_use]
    pub fn consumer_count(&self) -> usize {
        self.shared.queues.len()
    }

    fn first_full_queue(&self) -> Option<usize> {
        self.shared.queues.iter().position(ArrayQueue::is_full)
    }

    fn push_to_all(&self, value: T) {
        debug_assert!(!self.shared.queues.is_empty());
        let last = self.shared.queues.len() - 1;

        for idx in 0..last {
            assert!(
                self.shared.queues[idx].push(value.clone()).is_ok(),
                "SPMC queue should have capacity after precheck"
            );
            self.shared.not_empty[idx].notify_one();
        }

        assert!(
            self.shared.queues[last].push(value).is_ok(),
            "SPMC queue should have capacity after precheck"
        );
        self.shared.not_empty[last].notify_one();
    }
}

impl<T> Drop for SpmcProducer<T> {
    fn drop(&mut self) {
        if !self.shared.closed.swap(true, Ordering::AcqRel) {
            for notify in &self.shared.not_empty {
                notify.notify_waiters();
            }
            for notify in &self.shared.not_full {
                notify.notify_waiters();
            }
        }
    }
}

/// Consumer side of the SPMC broadcast channel.
pub struct SpmcConsumer<T> {
    shared: Arc<SpmcShared<T>>,
    consumer_idx: usize,
}

impl<T> SpmcConsumer<T> {
    /// Receive one value, waiting asynchronously until this consumer queue has data.
    ///
    /// Returns `None` once the channel is closed and this consumer queue is drained.
    pub async fn recv(&self) -> Option<T> {
        loop {
            if let Some(value) = self.try_recv() {
                return Some(value);
            }

            if self.is_closed() {
                return None;
            }

            let notified = self.shared.not_empty[self.consumer_idx].notified();
            if self.shared.queues[self.consumer_idx].is_empty() && !self.is_closed() {
                notified.await;
            }
        }
    }

    /// Try to receive one value without waiting.
    pub fn try_recv(&self) -> Option<T> {
        let value = self.shared.queues[self.consumer_idx].pop();
        if value.is_some() {
            self.shared.not_full[self.consumer_idx].notify_one();
        }
        value
    }

    /// Returns true if the channel is closed.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::Acquire)
    }

    /// Current queue depth for this consumer.
    #[must_use]
    pub fn depth(&self) -> usize {
        self.shared.queues[self.consumer_idx].len()
    }

    /// Queue capacity for this consumer.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.shared.queues[self.consumer_idx].capacity()
    }

    /// Index of this consumer in the channel.
    #[must_use]
    pub const fn consumer_index(&self) -> usize {
        self.consumer_idx
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{channel, spmc_channel};

    #[tokio::test]
    async fn preserves_fifo_order() {
        let (tx, rx) = channel(8);
        tx.send(1).await.unwrap();
        tx.send(2).await.unwrap();
        tx.send(3).await.unwrap();

        assert_eq!(rx.recv().await, Some(1));
        assert_eq!(rx.recv().await, Some(2));
        assert_eq!(rx.recv().await, Some(3));
    }

    #[tokio::test]
    async fn try_send_respects_capacity() {
        let (tx, rx) = channel(1);
        assert!(tx.try_send(11).is_ok());
        assert!(tx.try_send(12).is_err());
        assert_eq!(rx.recv().await, Some(11));
        assert!(tx.try_send(13).is_ok());
        assert_eq!(rx.recv().await, Some(13));
    }

    #[tokio::test]
    async fn recv_returns_none_after_close_and_drain() {
        let (tx, rx) = channel(2);
        tx.send(1).await.unwrap();
        tx.send(2).await.unwrap();
        drop(tx);

        assert_eq!(rx.recv().await, Some(1));
        assert_eq!(rx.recv().await, Some(2));
        assert_eq!(rx.recv().await, None);
    }

    #[test]
    #[should_panic(expected = "SPSC capacity must be > 0")]
    fn zero_capacity_panics() {
        let (_, _) = channel::<u8>(0);
    }

    #[test]
    fn try_recv_on_empty_returns_none() {
        let (_tx, rx) = channel::<u32>(4);
        assert_eq!(rx.try_recv(), None);
    }

    #[test]
    fn depth_and_capacity_methods() {
        let (tx, rx) = channel::<u32>(4);
        assert_eq!(tx.capacity(), 4);
        assert_eq!(rx.capacity(), 4);
        assert_eq!(tx.depth(), 0);
        assert_eq!(rx.depth(), 0);

        tx.try_send(1).unwrap();
        tx.try_send(2).unwrap();
        assert_eq!(tx.depth(), 2);
        assert_eq!(rx.depth(), 2);

        rx.try_recv();
        assert_eq!(tx.depth(), 1);
        assert_eq!(rx.depth(), 1);
    }

    #[test]
    fn close_from_producer_side() {
        let (tx, rx) = channel::<u32>(4);
        tx.try_send(1).unwrap();
        tx.close();

        assert!(tx.is_closed());
        assert!(rx.is_closed());

        // Items already in buffer can still be received.
        assert_eq!(rx.try_recv(), Some(1));
        assert_eq!(rx.try_recv(), None);
    }

    #[test]
    fn try_send_on_closed_returns_err() {
        let (tx, _rx) = channel::<u32>(4);
        tx.close();
        assert!(tx.try_send(42).is_err());
    }

    #[test]
    fn drop_producer_closes_channel() {
        let (tx, rx) = channel::<u32>(4);
        tx.try_send(99).unwrap();
        drop(tx);
        assert!(rx.is_closed());
        // Drain remaining.
        assert_eq!(rx.try_recv(), Some(99));
        assert_eq!(rx.try_recv(), None);
    }

    #[tokio::test]
    async fn recv_on_closed_empty_returns_none() {
        let (tx, rx) = channel::<u32>(4);
        drop(tx);
        assert_eq!(rx.recv().await, None);
    }

    #[tokio::test]
    async fn send_on_closed_returns_err() {
        let (tx, rx) = channel::<u32>(4);
        drop(rx);
        tx.close(); // Consumer drop doesn't auto-close.
        let result = tx.send(1).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn fill_and_drain_multiple_cycles() {
        let (tx, rx) = channel::<u32>(2);
        for cycle in 0..5u32 {
            let base = cycle * 2;
            tx.send(base).await.unwrap();
            tx.send(base + 1).await.unwrap();
            assert_eq!(rx.recv().await, Some(base));
            assert_eq!(rx.recv().await, Some(base + 1));
        }
    }

    #[tokio::test]
    async fn send_waits_until_consumer_frees_capacity() {
        let (tx, rx) = channel(1);
        tx.send(1).await.unwrap();

        let sender = crate::runtime_compat::task::spawn(async move { tx.send(2).await });

        crate::runtime_compat::sleep(Duration::from_millis(20)).await;
        assert!(!sender.is_finished());

        assert_eq!(rx.recv().await, Some(1));
        assert!(sender.await.unwrap().is_ok());
        assert_eq!(rx.recv().await, Some(2));
    }

    // ----------------------------------------------------------------
    // Additional coverage
    // ----------------------------------------------------------------

    #[test]
    fn try_send_returns_value_on_full() {
        let (tx, _rx) = channel::<u32>(1);
        tx.try_send(10).unwrap();
        let err = tx.try_send(20).unwrap_err();
        assert_eq!(err, 20, "try_send should return the rejected value");
    }

    #[test]
    fn try_send_returns_value_on_closed() {
        let (tx, _rx) = channel::<String>(4);
        tx.close();
        let err = tx.try_send("hello".to_string()).unwrap_err();
        assert_eq!(err, "hello", "try_send should return the value on closed");
    }

    #[test]
    fn close_is_idempotent() {
        let (tx, rx) = channel::<u32>(4);
        tx.close();
        assert!(tx.is_closed());
        assert!(rx.is_closed());
        // Second close should not panic or change state
        tx.close();
        assert!(tx.is_closed());
        assert!(rx.is_closed());
    }

    #[tokio::test]
    async fn try_recv_drains_after_producer_drop() {
        let (tx, rx) = channel::<u32>(4);
        tx.try_send(1).unwrap();
        tx.try_send(2).unwrap();
        tx.try_send(3).unwrap();
        drop(tx);

        assert!(rx.is_closed());
        assert_eq!(rx.try_recv(), Some(1));
        assert_eq!(rx.try_recv(), Some(2));
        assert_eq!(rx.try_recv(), Some(3));
        assert_eq!(rx.try_recv(), None);
    }

    #[tokio::test]
    async fn large_batch_1000_items() {
        let (tx, rx) = channel(64);
        let sender = crate::runtime_compat::task::spawn(async move {
            for i in 0..1000u32 {
                tx.send(i).await.unwrap();
            }
        });

        for i in 0..1000u32 {
            let val = rx.recv().await.unwrap();
            assert_eq!(val, i);
        }
        sender.await.unwrap();
    }

    #[tokio::test]
    async fn string_payload() {
        let (tx, rx) = channel(4);
        tx.send("hello".to_string()).await.unwrap();
        tx.send("world".to_string()).await.unwrap();
        assert_eq!(rx.recv().await, Some("hello".to_string()));
        assert_eq!(rx.recv().await, Some("world".to_string()));
    }

    #[tokio::test]
    async fn vec_payload() {
        let (tx, rx) = channel(2);
        tx.send(vec![1, 2, 3]).await.unwrap();
        tx.send(vec![4, 5]).await.unwrap();
        assert_eq!(rx.recv().await, Some(vec![1, 2, 3]));
        assert_eq!(rx.recv().await, Some(vec![4, 5]));
    }

    #[test]
    fn depth_updates_after_try_recv() {
        let (tx, rx) = channel::<u32>(4);
        tx.try_send(1).unwrap();
        tx.try_send(2).unwrap();
        tx.try_send(3).unwrap();
        assert_eq!(tx.depth(), 3);
        assert_eq!(rx.depth(), 3);

        rx.try_recv();
        assert_eq!(tx.depth(), 2);
        assert_eq!(rx.depth(), 2);

        rx.try_recv();
        rx.try_recv();
        assert_eq!(tx.depth(), 0);
        assert_eq!(rx.depth(), 0);
    }

    #[tokio::test]
    async fn capacity_1_stress() {
        let (tx, rx) = channel(1);
        let sender = crate::runtime_compat::task::spawn(async move {
            for i in 0..100u32 {
                tx.send(i).await.unwrap();
            }
        });

        for i in 0..100u32 {
            let val = rx.recv().await.unwrap();
            assert_eq!(val, i);
        }
        sender.await.unwrap();
    }

    #[test]
    fn fill_to_exact_capacity() {
        let (tx, rx) = channel::<u32>(3);
        assert!(tx.try_send(1).is_ok());
        assert!(tx.try_send(2).is_ok());
        assert!(tx.try_send(3).is_ok());
        assert!(tx.try_send(4).is_err()); // Full
        assert_eq!(tx.depth(), 3);
        assert_eq!(rx.depth(), 3);
    }

    #[tokio::test]
    async fn alternating_send_recv() {
        let (tx, rx) = channel(2);
        for i in 0..50u32 {
            tx.send(i).await.unwrap();
            assert_eq!(rx.recv().await, Some(i));
        }
        assert_eq!(tx.depth(), 0);
    }

    #[tokio::test]
    async fn send_on_closed_returns_original_value() {
        let (tx, _rx) = channel::<u32>(4);
        tx.close();
        let result = tx.send(42).await;
        assert_eq!(result.unwrap_err(), 42);
    }

    #[tokio::test]
    async fn recv_returns_none_immediately_on_empty_closed() {
        let (tx, rx) = channel::<u32>(4);
        tx.close();
        let start = std::time::Instant::now();
        let result = rx.recv().await;
        assert!(result.is_none());
        // Should return almost immediately, not block
        assert!(start.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn multiple_try_send_fill_drain_partial() {
        let (tx, rx) = channel::<u32>(2);
        // Fill
        assert!(tx.try_send(1).is_ok());
        assert!(tx.try_send(2).is_ok());
        assert!(tx.try_send(3).is_err()); // Full

        // Drain one
        assert_eq!(rx.try_recv(), Some(1));

        // Now one slot is free
        assert!(tx.try_send(3).is_ok());
        assert!(tx.try_send(4).is_err()); // Full again

        assert_eq!(rx.try_recv(), Some(2));
        assert_eq!(rx.try_recv(), Some(3));
        assert_eq!(rx.try_recv(), None);
    }

    #[tokio::test]
    async fn concurrent_producer_consumer_stress() {
        let (tx, rx) = channel(16);
        let n = 5000u32;

        let producer = crate::runtime_compat::task::spawn(async move {
            for i in 0..n {
                tx.send(i).await.unwrap();
            }
        });

        let consumer = crate::runtime_compat::task::spawn(async move {
            let mut received = Vec::with_capacity(n as usize);
            for _ in 0..n {
                received.push(rx.recv().await.unwrap());
            }
            received
        });

        producer.await.unwrap();
        let received = consumer.await.unwrap();
        assert_eq!(received.len(), n as usize);
        // FIFO order preserved
        for (i, &v) in received.iter().enumerate() {
            assert_eq!(v, i as u32);
        }
    }

    #[tokio::test]
    async fn recv_wakes_on_close() {
        let (tx, rx) = channel::<u32>(4);

        let consumer = crate::runtime_compat::task::spawn(async move { rx.recv().await });

        // Give consumer time to block on empty queue
        crate::runtime_compat::sleep(Duration::from_millis(20)).await;

        // Close should wake the consumer
        tx.close();
        let result = consumer.await.unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn producer_and_consumer_capacity_agree() {
        let (tx, rx) = channel::<u8>(42);
        assert_eq!(tx.capacity(), rx.capacity());
        assert_eq!(tx.capacity(), 42);
    }

    #[test]
    fn producer_and_consumer_depth_agree() {
        let (tx, rx) = channel::<u32>(8);
        assert_eq!(tx.depth(), rx.depth());
        tx.try_send(1).unwrap();
        assert_eq!(tx.depth(), rx.depth());
        tx.try_send(2).unwrap();
        assert_eq!(tx.depth(), rx.depth());
        rx.try_recv();
        assert_eq!(tx.depth(), rx.depth());
    }

    #[tokio::test]
    async fn spmc_broadcasts_to_all_consumers_in_order() {
        // Keep enough headroom so this test validates ordering, not backpressure.
        let (tx, mut consumers) = spmc_channel(16, 2);
        let rx0 = consumers.remove(0);
        let rx1 = consumers.remove(0);

        for i in 0..10u32 {
            tx.send(i).await.unwrap();
        }

        for i in 0..10u32 {
            assert_eq!(rx0.recv().await, Some(i));
            assert_eq!(rx1.recv().await, Some(i));
        }
    }

    #[tokio::test]
    async fn spmc_send_waits_for_slowest_consumer() {
        let (tx, mut consumers) = spmc_channel(1, 2);
        let rx0 = consumers.remove(0);
        let rx1 = consumers.remove(0);

        tx.send(1u32).await.unwrap();
        let sender = crate::runtime_compat::task::spawn(async move { tx.send(2u32).await });

        crate::runtime_compat::sleep(Duration::from_millis(20)).await;
        assert!(!sender.is_finished());

        assert_eq!(rx0.recv().await, Some(1));
        crate::runtime_compat::sleep(Duration::from_millis(20)).await;
        assert!(!sender.is_finished());

        assert_eq!(rx1.recv().await, Some(1));
        assert!(sender.await.unwrap().is_ok());

        assert_eq!(rx0.recv().await, Some(2));
        assert_eq!(rx1.recv().await, Some(2));
    }

    #[test]
    fn spmc_try_send_fails_when_any_consumer_is_full() {
        let (tx, mut consumers) = spmc_channel(1, 2);
        let rx0 = consumers.remove(0);
        let rx1 = consumers.remove(0);

        assert!(tx.try_send(1u32).is_ok());
        // Drain only one consumer; other queue remains full.
        assert_eq!(rx0.try_recv(), Some(1));
        assert_eq!(tx.try_send(2u32), Err(2));

        // Once slow consumer drains, send can proceed.
        assert_eq!(rx1.try_recv(), Some(1));
        assert!(tx.try_send(2u32).is_ok());
        assert_eq!(rx0.try_recv(), Some(2));
        assert_eq!(rx1.try_recv(), Some(2));
    }

    #[tokio::test]
    async fn spmc_close_allows_drain_then_none() {
        let (tx, consumers) = spmc_channel(2, 2);
        tx.send(7u32).await.unwrap();
        drop(tx);

        for rx in consumers {
            assert_eq!(rx.recv().await, Some(7));
            assert_eq!(rx.recv().await, None);
        }
    }

    #[test]
    #[should_panic(expected = "SPMC capacity must be > 0")]
    fn spmc_zero_capacity_panics() {
        let _ = spmc_channel::<u8>(0, 1);
    }

    #[test]
    #[should_panic(expected = "SPMC consumers must be > 0")]
    fn spmc_zero_consumers_panics() {
        let _ = spmc_channel::<u8>(8, 0);
    }
}
