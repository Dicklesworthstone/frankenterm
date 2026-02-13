//! Fixed-capacity ring buffer for bounded circular storage.
//!
//! A ring buffer (circular buffer) maintains a fixed-size window of the most
//! recent items. When full, new items overwrite the oldest. No allocations
//! after initial creation.
//!
//! # Use cases in FrankenTerm
//!
//! - **Output line history**: Keep last N output lines per pane for pattern matching.
//! - **Event windows**: Sliding window of recent events for anomaly detection.
//! - **Metric history**: Rolling history of snapshots for trend analysis.
//! - **Replay buffer**: Fixed-size buffer of recent actions for undo/replay.

use serde::{Deserialize, Serialize};

// =============================================================================
// RingBuffer
// =============================================================================

/// A fixed-capacity ring buffer.
///
/// When the buffer is full, new items overwrite the oldest items.
/// Iteration yields items from oldest to newest.
///
/// # Example
///
/// ```ignore
/// let mut rb = RingBuffer::new(3);
/// rb.push(1);
/// rb.push(2);
/// rb.push(3);
/// rb.push(4); // overwrites 1
/// assert_eq!(rb.iter().collect::<Vec<_>>(), vec![&2, &3, &4]);
/// ```
pub struct RingBuffer<T> {
    buf: Vec<Option<T>>,
    capacity: usize,
    head: usize, // next write position
    len: usize,  // current number of items
    total: u64,  // total items ever pushed
}

impl<T> RingBuffer<T> {
    /// Create a new ring buffer with the given capacity.
    ///
    /// # Panics
    ///
    /// Panics if `capacity` is 0.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "capacity must be > 0");
        let mut buf = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            buf.push(None);
        }
        Self {
            buf,
            capacity,
            head: 0,
            len: 0,
            total: 0,
        }
    }

    /// Push an item into the buffer.
    ///
    /// If full, the oldest item is overwritten and returned.
    pub fn push(&mut self, item: T) -> Option<T> {
        let evicted = self.buf[self.head].take();
        self.buf[self.head] = Some(item);
        self.head = (self.head + 1) % self.capacity;
        self.total += 1;
        if self.len < self.capacity {
            self.len += 1;
            None
        } else {
            evicted
        }
    }

    /// Get the most recently pushed item.
    #[must_use]
    pub fn back(&self) -> Option<&T> {
        if self.len == 0 {
            return None;
        }
        let idx = if self.head == 0 {
            self.capacity - 1
        } else {
            self.head - 1
        };
        self.buf[idx].as_ref()
    }

    /// Get the oldest item in the buffer.
    #[must_use]
    pub fn front(&self) -> Option<&T> {
        if self.len == 0 {
            return None;
        }
        let start = if self.len < self.capacity {
            0
        } else {
            self.head
        };
        self.buf[start].as_ref()
    }

    /// Iterate from oldest to newest.
    pub fn iter(&self) -> RingBufferIter<'_, T> {
        let start = if self.len < self.capacity {
            0
        } else {
            self.head
        };
        RingBufferIter {
            buf: &self.buf,
            capacity: self.capacity,
            pos: start,
            remaining: self.len,
        }
    }

    /// Get item at logical index (0 = oldest).
    #[must_use]
    pub fn get(&self, index: usize) -> Option<&T> {
        if index >= self.len {
            return None;
        }
        let start = if self.len < self.capacity {
            0
        } else {
            self.head
        };
        let actual = (start + index) % self.capacity;
        self.buf[actual].as_ref()
    }

    /// Current number of items.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the buffer is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether the buffer is full.
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.len == self.capacity
    }

    /// Maximum capacity.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Total items ever pushed (including overwrites).
    #[must_use]
    pub fn total_pushed(&self) -> u64 {
        self.total
    }

    /// Total items that were overwritten.
    #[must_use]
    pub fn total_evicted(&self) -> u64 {
        if self.total > self.capacity as u64 {
            self.total - self.capacity as u64
        } else {
            0
        }
    }

    /// Clear all items.
    pub fn clear(&mut self) {
        for slot in self.buf.iter_mut() {
            *slot = None;
        }
        self.head = 0;
        self.len = 0;
    }

    /// Drain all items from oldest to newest, leaving the buffer empty.
    pub fn drain(&mut self) -> Vec<T> {
        let mut result = Vec::with_capacity(self.len);
        let start = if self.len < self.capacity {
            0
        } else {
            self.head
        };
        for i in 0..self.len {
            let idx = (start + i) % self.capacity;
            if let Some(item) = self.buf[idx].take() {
                result.push(item);
            }
        }
        self.head = 0;
        self.len = 0;
        result
    }

    /// Collect to a Vec (oldest to newest) without draining.
    #[must_use]
    pub fn to_vec(&self) -> Vec<&T> {
        self.iter().collect()
    }
}

impl<T: Clone> RingBuffer<T> {
    /// Collect to an owned Vec (oldest to newest).
    #[must_use]
    pub fn to_owned_vec(&self) -> Vec<T> {
        self.iter().cloned().collect()
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for RingBuffer<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RingBuffer")
            .field("capacity", &self.capacity)
            .field("len", &self.len)
            .field("total_pushed", &self.total)
            .finish()
    }
}

// =============================================================================
// RingBufferIter
// =============================================================================

/// Iterator over ring buffer items (oldest to newest).
pub struct RingBufferIter<'a, T> {
    buf: &'a [Option<T>],
    capacity: usize,
    pos: usize,
    remaining: usize,
}

impl<'a, T> Iterator for RingBufferIter<'a, T> {
    type Item = &'a T;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let item = self.buf[self.pos].as_ref();
        self.pos = (self.pos + 1) % self.capacity;
        self.remaining -= 1;
        item
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl<'a, T> ExactSizeIterator for RingBufferIter<'a, T> {}

// =============================================================================
// RingBufferStats (serializable)
// =============================================================================

/// Serializable statistics about a ring buffer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RingBufferStats {
    /// Maximum capacity.
    pub capacity: usize,
    /// Current number of items.
    pub len: usize,
    /// Total items ever pushed.
    pub total_pushed: u64,
    /// Total items evicted (overwritten).
    pub total_evicted: u64,
    /// Fill ratio (len / capacity).
    pub fill_ratio: f64,
}

impl<T> RingBuffer<T> {
    /// Get statistics.
    #[must_use]
    pub fn stats(&self) -> RingBufferStats {
        RingBufferStats {
            capacity: self.capacity,
            len: self.len,
            total_pushed: self.total,
            total_evicted: self.total_evicted(),
            fill_ratio: self.len as f64 / self.capacity as f64,
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -- Basic operations -------------------------------------------------------

    #[test]
    fn new_buffer_is_empty() {
        let rb: RingBuffer<i32> = RingBuffer::new(5);
        assert!(rb.is_empty());
        assert!(!rb.is_full());
        assert_eq!(rb.len(), 0);
        assert_eq!(rb.capacity(), 5);
    }

    #[test]
    fn push_and_len() {
        let mut rb = RingBuffer::new(3);
        assert_eq!(rb.push(1), None);
        assert_eq!(rb.push(2), None);
        assert_eq!(rb.push(3), None);
        assert_eq!(rb.len(), 3);
        assert!(rb.is_full());
    }

    #[test]
    fn push_overwrites_oldest() {
        let mut rb = RingBuffer::new(3);
        rb.push(1);
        rb.push(2);
        rb.push(3);
        let evicted = rb.push(4);
        assert_eq!(evicted, Some(1));
        assert_eq!(rb.len(), 3);
    }

    #[test]
    fn push_returns_evicted() {
        let mut rb = RingBuffer::new(2);
        assert_eq!(rb.push(10), None);
        assert_eq!(rb.push(20), None);
        assert_eq!(rb.push(30), Some(10));
        assert_eq!(rb.push(40), Some(20));
    }

    // -- Access -----------------------------------------------------------------

    #[test]
    fn front_and_back() {
        let mut rb = RingBuffer::new(3);
        assert_eq!(rb.front(), None);
        assert_eq!(rb.back(), None);

        rb.push(1);
        assert_eq!(rb.front(), Some(&1));
        assert_eq!(rb.back(), Some(&1));

        rb.push(2);
        rb.push(3);
        assert_eq!(rb.front(), Some(&1));
        assert_eq!(rb.back(), Some(&3));

        rb.push(4); // evicts 1
        assert_eq!(rb.front(), Some(&2));
        assert_eq!(rb.back(), Some(&4));
    }

    #[test]
    fn get_by_index() {
        let mut rb = RingBuffer::new(3);
        rb.push(10);
        rb.push(20);
        rb.push(30);

        assert_eq!(rb.get(0), Some(&10)); // oldest
        assert_eq!(rb.get(1), Some(&20));
        assert_eq!(rb.get(2), Some(&30)); // newest
        assert_eq!(rb.get(3), None); // out of bounds
    }

    #[test]
    fn get_after_wrap() {
        let mut rb = RingBuffer::new(3);
        rb.push(1);
        rb.push(2);
        rb.push(3);
        rb.push(4); // evicts 1
        rb.push(5); // evicts 2

        assert_eq!(rb.get(0), Some(&3)); // oldest
        assert_eq!(rb.get(1), Some(&4));
        assert_eq!(rb.get(2), Some(&5)); // newest
    }

    // -- Iteration --------------------------------------------------------------

    #[test]
    fn iter_before_full() {
        let mut rb = RingBuffer::new(5);
        rb.push(1);
        rb.push(2);
        rb.push(3);
        let v: Vec<&i32> = rb.iter().collect();
        assert_eq!(v, vec![&1, &2, &3]);
    }

    #[test]
    fn iter_after_wrap() {
        let mut rb = RingBuffer::new(3);
        for i in 1..=5 {
            rb.push(i);
        }
        let v: Vec<&i32> = rb.iter().collect();
        assert_eq!(v, vec![&3, &4, &5]);
    }

    #[test]
    fn iter_empty() {
        let rb: RingBuffer<i32> = RingBuffer::new(3);
        let v: Vec<&i32> = rb.iter().collect();
        assert!(v.is_empty());
    }

    #[test]
    fn iter_exact_size() {
        let mut rb = RingBuffer::new(5);
        rb.push(1);
        rb.push(2);
        assert_eq!(rb.iter().len(), 2);
    }

    #[test]
    fn to_vec() {
        let mut rb = RingBuffer::new(3);
        rb.push(10);
        rb.push(20);
        rb.push(30);
        rb.push(40);
        assert_eq!(rb.to_vec(), vec![&20, &30, &40]);
    }

    #[test]
    fn to_owned_vec() {
        let mut rb = RingBuffer::new(2);
        rb.push(String::from("a"));
        rb.push(String::from("b"));
        rb.push(String::from("c"));
        let owned = rb.to_owned_vec();
        assert_eq!(owned, vec!["b".to_string(), "c".to_string()]);
    }

    // -- Clear and drain --------------------------------------------------------

    #[test]
    fn clear() {
        let mut rb = RingBuffer::new(3);
        rb.push(1);
        rb.push(2);
        rb.clear();
        assert!(rb.is_empty());
        assert_eq!(rb.len(), 0);
        assert_eq!(rb.front(), None);
    }

    #[test]
    fn drain() {
        let mut rb = RingBuffer::new(3);
        rb.push(1);
        rb.push(2);
        rb.push(3);
        rb.push(4); // evicts 1
        let drained = rb.drain();
        assert_eq!(drained, vec![2, 3, 4]);
        assert!(rb.is_empty());
    }

    // -- Stats ------------------------------------------------------------------

    #[test]
    fn stats() {
        let mut rb = RingBuffer::new(3);
        rb.push(1);
        rb.push(2);
        rb.push(3);
        rb.push(4);
        rb.push(5);

        let s = rb.stats();
        assert_eq!(s.capacity, 3);
        assert_eq!(s.len, 3);
        assert_eq!(s.total_pushed, 5);
        assert_eq!(s.total_evicted, 2);
        assert!((s.fill_ratio - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn stats_serde_roundtrip() {
        let s = RingBufferStats {
            capacity: 10,
            len: 5,
            total_pushed: 100,
            total_evicted: 90,
            fill_ratio: 0.5,
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: RingBufferStats = serde_json::from_str(&json).unwrap();
        assert_eq!(s.capacity, back.capacity);
        assert_eq!(s.total_pushed, back.total_pushed);
    }

    // -- Total tracking ---------------------------------------------------------

    #[test]
    fn total_pushed() {
        let mut rb = RingBuffer::new(2);
        for i in 0..10 {
            rb.push(i);
        }
        assert_eq!(rb.total_pushed(), 10);
        assert_eq!(rb.total_evicted(), 8);
    }

    // -- Edge cases -------------------------------------------------------------

    #[test]
    fn capacity_one() {
        let mut rb = RingBuffer::new(1);
        assert_eq!(rb.push(1), None);
        assert_eq!(rb.push(2), Some(1));
        assert_eq!(rb.push(3), Some(2));
        assert_eq!(rb.len(), 1);
        assert_eq!(rb.back(), Some(&3));
        assert_eq!(rb.front(), Some(&3));
    }

    #[test]
    #[should_panic(expected = "capacity must be > 0")]
    fn zero_capacity_panics() {
        let _rb: RingBuffer<i32> = RingBuffer::new(0);
    }

    #[test]
    fn debug_format() {
        let rb: RingBuffer<i32> = RingBuffer::new(5);
        let s = format!("{rb:?}");
        assert!(s.contains("RingBuffer"));
        assert!(s.contains("capacity"));
    }

    #[test]
    fn many_wraps() {
        let mut rb = RingBuffer::new(3);
        for i in 0..1000 {
            rb.push(i);
        }
        assert_eq!(rb.len(), 3);
        assert_eq!(rb.to_vec(), vec![&997, &998, &999]);
    }

    #[test]
    fn push_after_clear() {
        let mut rb = RingBuffer::new(3);
        rb.push(1);
        rb.push(2);
        rb.clear();
        rb.push(10);
        rb.push(20);
        assert_eq!(rb.to_vec(), vec![&10, &20]);
    }
}
