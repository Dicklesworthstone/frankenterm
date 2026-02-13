//! Embedding worker — processes embedding jobs from a queue.

use std::sync::atomic::{AtomicU64, Ordering};

/// Worker that processes embedding requests from a queue.
pub struct EmbedWorker {
    id: u32,
    processed: AtomicU64,
}

impl EmbedWorker {
    /// Create a new worker with the given ID.
    pub fn new(id: u32) -> Self {
        Self {
            id,
            processed: AtomicU64::new(0),
        }
    }

    /// Get the worker ID.
    pub fn id(&self) -> u32 {
        self.id
    }

    /// Get the number of processed requests.
    pub fn processed(&self) -> u64 {
        self.processed.load(Ordering::Relaxed)
    }

    /// Increment the processed counter.
    pub fn increment(&self) {
        self.processed.fetch_add(1, Ordering::Relaxed);
    }
}
