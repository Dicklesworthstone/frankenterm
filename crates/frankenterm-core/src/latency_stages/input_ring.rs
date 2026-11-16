use std::fmt;

use serde::{Deserialize, Serialize};

use super::LatencyStage;

// B2: Bounded Input Ring
//
// Fixed-capacity FIFO ring for the input lane with backpressure.
// Operations are O(1) amortized, bounded in time; no allocation on enqueue.
// AARSP Bead: ft-2p9cb.2.2.1

/// An item in the input ring.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputRingItem {
    /// Sequence number (monotonically increasing).
    pub seq: u64,
    /// Pipeline stage this item is for.
    pub stage: LatencyStage,
    /// Estimated latency cost in microseconds.
    pub estimated_cost_us: f64,
    /// Correlation ID.
    pub correlation_id: String,
    /// Arrival timestamp in microseconds from epoch.
    pub arrived_us: u64,
    /// Deadline (0 = none).
    pub deadline_us: u64,
}

/// Backpressure signal from the input ring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RingBackpressure {
    /// Ring has capacity, accept freely.
    Accept,
    /// Ring is above high-water mark; signal producer to slow down.
    SlowDown,
    /// Ring is full; reject or drop.
    Full,
}

impl fmt::Display for RingBackpressure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Accept => write!(f, "ACCEPT"),
            Self::SlowDown => write!(f, "SLOW_DOWN"),
            Self::Full => write!(f, "FULL"),
        }
    }
}

/// Configuration for the bounded input ring.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputRingConfig {
    /// Fixed capacity of the ring.
    pub capacity: usize,
    /// High-water mark fraction (0.0..1.0) above which backpressure = SlowDown.
    pub high_water_mark: f64,
    /// Whether to track per-item latency from arrival to dequeue.
    pub track_sojourn: bool,
}

impl Default for InputRingConfig {
    fn default() -> Self {
        Self {
            capacity: 256,
            high_water_mark: 0.75,
            track_sojourn: true,
        }
    }
}

/// Diagnostic snapshot of the input ring.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputRingSnapshot {
    pub capacity: usize,
    pub len: usize,
    pub total_enqueued: u64,
    pub total_dequeued: u64,
    pub total_dropped: u64,
    pub backpressure: RingBackpressure,
    pub head_seq: u64,
    pub tail_seq: u64,
    pub sojourn_mean_us: Option<f64>,
}

/// Bounded FIFO ring for the input lane.
///
/// # Invariants
///
/// 1. `len <= capacity` always.
/// 2. `head_seq <= tail_seq` (head is next to dequeue, tail is next to enqueue).
/// 3. `total_enqueued = total_dequeued + total_dropped + len`.
/// 4. O(1) enqueue and dequeue.
/// 5. Deterministic: same sequence of ops -> same state.
#[derive(Debug, Clone)]
pub struct InputRing {
    config: InputRingConfig,
    buffer: Vec<Option<InputRingItem>>,
    head: usize,
    tail: usize,
    len: usize,
    next_seq: u64,
    pub(super) total_enqueued: u64,
    pub(super) total_dequeued: u64,
    pub(super) total_dropped: u64,
    sojourn_sum_us: f64,
    sojourn_count: u64,
}

impl InputRing {
    /// Create a new input ring with the given configuration.
    pub fn new(config: InputRingConfig) -> Self {
        let cap = config.capacity.max(1);
        Self {
            buffer: (0..cap).map(|_| None).collect(),
            config: InputRingConfig {
                capacity: cap,
                ..config
            },
            head: 0,
            tail: 0,
            len: 0,
            next_seq: 1,
            total_enqueued: 0,
            total_dequeued: 0,
            total_dropped: 0,
            sojourn_sum_us: 0.0,
            sojourn_count: 0,
        }
    }

    /// Create a ring with default config.
    pub fn with_defaults() -> Self {
        Self::new(InputRingConfig::default())
    }

    /// Current number of items in the ring.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Is the ring empty?
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Is the ring full?
    pub fn is_full(&self) -> bool {
        self.len >= self.config.capacity
    }

    /// Current backpressure signal.
    pub fn backpressure(&self) -> RingBackpressure {
        if self.is_full() {
            RingBackpressure::Full
        } else if self.len as f64 / self.config.capacity as f64 >= self.config.high_water_mark {
            RingBackpressure::SlowDown
        } else {
            RingBackpressure::Accept
        }
    }

    /// Enqueue an item. Returns Ok(seq) on success, Err(backpressure) if full.
    pub fn enqueue(
        &mut self,
        stage: LatencyStage,
        estimated_cost_us: f64,
        correlation_id: &str,
        arrived_us: u64,
        deadline_us: u64,
    ) -> Result<u64, RingBackpressure> {
        if self.is_full() {
            self.total_dropped += 1;
            return Err(RingBackpressure::Full);
        }

        let seq = self.next_seq;
        self.next_seq += 1;

        self.buffer[self.tail] = Some(InputRingItem {
            seq,
            stage,
            estimated_cost_us,
            correlation_id: correlation_id.to_string(),
            arrived_us,
            deadline_us,
        });
        self.tail = (self.tail + 1) % self.config.capacity;
        self.len += 1;
        self.total_enqueued += 1;

        Ok(seq)
    }

    /// Dequeue the oldest item. Returns None if empty.
    pub fn dequeue(&mut self, now_us: u64) -> Option<InputRingItem> {
        if self.is_empty() {
            return None;
        }

        let item = self.buffer[self.head].take()?;
        self.head = (self.head + 1) % self.config.capacity;
        self.len -= 1;
        self.total_dequeued += 1;

        if self.config.track_sojourn && now_us >= item.arrived_us {
            self.sojourn_sum_us += (now_us - item.arrived_us) as f64;
            self.sojourn_count += 1;
        }

        Some(item)
    }

    /// Peek at the head item without removing it.
    pub fn peek(&self) -> Option<&InputRingItem> {
        if self.is_empty() {
            None
        } else {
            self.buffer[self.head].as_ref()
        }
    }

    /// Mean sojourn time (time in ring) in microseconds, if tracked.
    pub fn mean_sojourn_us(&self) -> Option<f64> {
        if self.sojourn_count > 0 {
            Some(self.sojourn_sum_us / self.sojourn_count as f64)
        } else {
            None
        }
    }

    /// Diagnostic snapshot.
    pub fn snapshot(&self) -> InputRingSnapshot {
        InputRingSnapshot {
            capacity: self.config.capacity,
            len: self.len,
            total_enqueued: self.total_enqueued,
            total_dequeued: self.total_dequeued,
            total_dropped: self.total_dropped,
            backpressure: self.backpressure(),
            head_seq: self.peek().map(|i| i.seq).unwrap_or(self.next_seq),
            tail_seq: self.next_seq,
            sojourn_mean_us: self.mean_sojourn_us(),
        }
    }

    /// Status line for logging.
    pub fn status_line(&self) -> String {
        format!(
            "input_ring len={}/{} bp={} enq={} deq={} drop={}",
            self.len,
            self.config.capacity,
            self.backpressure(),
            self.total_enqueued,
            self.total_dequeued,
            self.total_dropped,
        )
    }

    /// Batch dequeue up to `max` items. Returns items in FIFO order.
    pub fn drain(&mut self, max: usize, now_us: u64) -> Vec<InputRingItem> {
        let count = max.min(self.len);
        let mut items = Vec::with_capacity(count);
        for _ in 0..count {
            if let Some(item) = self.dequeue(now_us) {
                items.push(item);
            } else {
                break;
            }
        }
        items
    }

    /// Dequeue items that have passed their deadline.
    /// Expired items are returned so the caller can handle them (e.g. log, escalate).
    pub fn drain_expired(&mut self, now_us: u64) -> Vec<InputRingItem> {
        let mut expired = Vec::new();
        let mut remaining = Vec::new();

        // Drain all items, separate expired from still-valid.
        let all = self.drain(self.len, now_us);
        for item in all {
            if item.deadline_us > 0 && now_us > item.deadline_us {
                expired.push(item);
            } else {
                remaining.push(item);
            }
        }

        // Re-enqueue non-expired items.
        for item in remaining {
            // Direct re-insert (bypass normal enqueue to preserve seq numbers).
            if self.len < self.config.capacity {
                self.buffer[self.tail] = Some(item);
                self.tail = (self.tail + 1) % self.config.capacity;
                self.len += 1;
                // Adjust counters to compensate for the drain+re-enqueue.
                self.total_dequeued -= 1;
            }
        }

        expired
    }

    /// Utilization fraction (0.0 to 1.0).
    pub fn utilization(&self) -> f64 {
        self.len as f64 / self.config.capacity as f64
    }

    /// Capacity of the ring.
    pub fn capacity(&self) -> usize {
        self.config.capacity
    }
}
