//! Lock-free single-producer/single-consumer ring buffer channel.
//!
//! This module provides a bounded async channel that is explicitly intended for
//! one producer task and one consumer task. Internally it uses
//! `crossbeam::queue::ArrayQueue`, which provides lock-free bounded queue
//! operations without requiring unsafe code in this crate.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crossbeam::queue::ArrayQueue;
use tokio::sync::Notify;

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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::channel;

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

    #[tokio::test]
    async fn send_waits_until_consumer_frees_capacity() {
        let (tx, rx) = channel(1);
        tx.send(1).await.unwrap();

        let sender = tokio::spawn(async move { tx.send(2).await });

        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!sender.is_finished());

        assert_eq!(rx.recv().await, Some(1));
        assert!(sender.await.unwrap().is_ok());
        assert_eq!(rx.recv().await, Some(2));
    }
}
