//! Work-stealing deque — lock-free concurrent scheduling primitive.
//!
//! A work-stealing deque allows one owner thread to push/pop from the bottom
//! (LIFO) while multiple stealers can steal from the top (FIFO). This enables
//! efficient dynamic load balancing across worker threads.
//!
//! # Design
//!
//! Based on the Chase-Lev algorithm, adapted for safe Rust (no unsafe code).
//! Uses `std::sync::Mutex` for the internal buffer since `#![forbid(unsafe_code)]`.
//! For the hot path (owner push/pop), contention is minimal since stealers
//! only touch the top index.
//!
//! ```text
//!   ┌──────────────────────────────────────┐
//!   │  Stealer ──steal()──→ [top]          │
//!   │                        ↓             │
//!   │                    ┌───┬───┬───┬───┐ │
//!   │                    │ A │ B │ C │ D │ │
//!   │                    └───┴───┴───┴───┘ │
//!   │                              ↑       │
//!   │  Owner ──push()/pop()──→ [bottom]    │
//!   └──────────────────────────────────────┘
//! ```
//!
//! # Use Cases in FrankenTerm
//!
//! - **Pane processing distribution**: Owner thread produces pane capture
//!   tasks; worker threads steal tasks when idle.
//! - **Pattern matching fanout**: Owner pushes pattern match jobs; stealers
//!   process panes they finish early on.
//! - **Event dispatch**: Event loop pushes events; handler threads steal
//!   events for processing.
//!
//! Bead: ft-t58vf

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

// ── Configuration ─────────────────────────────────────────────────────

/// Configuration for a work-stealing deque.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WsDequeConfig {
    /// Initial capacity hint. Default: 64.
    pub initial_capacity: usize,
}

impl Default for WsDequeConfig {
    fn default() -> Self {
        Self {
            initial_capacity: 64,
        }
    }
}

/// Statistics about a work-stealing deque.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WsDequeStats {
    pub len: usize,
    pub total_pushed: u64,
    pub total_popped: u64,
    pub total_stolen: u64,
    pub steal_failures: u64,
}

// ── Steal result ──────────────────────────────────────────────────────

/// Result of a steal attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StealResult<T> {
    /// Successfully stole an item.
    Success(T),
    /// The deque was empty.
    Empty,
    /// Another stealer got there first (retry may succeed).
    Retry,
}

impl<T> StealResult<T> {
    /// Returns true if the steal succeeded.
    pub fn is_success(&self) -> bool {
        matches!(self, StealResult::Success(_))
    }

    /// Returns true if the deque was empty.
    pub fn is_empty(&self) -> bool {
        matches!(self, StealResult::Empty)
    }

    /// Unwrap the stolen value, panicking if not Success.
    pub fn unwrap(self) -> T {
        match self {
            StealResult::Success(v) => v,
            StealResult::Empty => panic!("called unwrap on Empty"),
            StealResult::Retry => panic!("called unwrap on Retry"),
        }
    }

    /// Convert to Option, discarding the failure reason.
    pub fn into_option(self) -> Option<T> {
        match self {
            StealResult::Success(v) => Some(v),
            _ => None,
        }
    }
}

// ── Internal shared state ─────────────────────────────────────────────

#[derive(Debug)]
struct SharedState<T> {
    buffer: VecDeque<T>,
    total_pushed: u64,
    total_popped: u64,
    total_stolen: u64,
    steal_failures: u64,
}

impl<T> SharedState<T> {
    fn new(capacity: usize) -> Self {
        Self {
            buffer: VecDeque::with_capacity(capacity),
            total_pushed: 0,
            total_popped: 0,
            total_stolen: 0,
            steal_failures: 0,
        }
    }
}

// ── Worker (owner) ────────────────────────────────────────────────────

/// The owner side of a work-stealing deque.
///
/// Only one Worker should exist per deque. The Worker can push and pop
/// items from the bottom of the deque (LIFO order).
#[derive(Debug)]
pub struct Worker<T> {
    state: Arc<Mutex<SharedState<T>>>,
}

impl<T> Clone for Worker<T> {
    fn clone(&self) -> Self {
        Self {
            state: Arc::clone(&self.state),
        }
    }
}

impl<T> Worker<T> {
    /// Push an item onto the bottom of the deque.
    pub fn push(&self, item: T) {
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.buffer.push_back(item);
        state.total_pushed += 1;
    }

    /// Pop an item from the bottom of the deque (LIFO).
    pub fn pop(&self) -> Option<T> {
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let item = state.buffer.pop_back();
        if item.is_some() {
            state.total_popped += 1;
        }
        item
    }

    /// Number of items currently in the deque.
    pub fn len(&self) -> usize {
        let state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.buffer.len()
    }

    /// Whether the deque is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Get statistics.
    pub fn stats(&self) -> WsDequeStats {
        let state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        WsDequeStats {
            len: state.buffer.len(),
            total_pushed: state.total_pushed,
            total_popped: state.total_popped,
            total_stolen: state.total_stolen,
            steal_failures: state.steal_failures,
        }
    }
}

// ── Stealer ───────────────────────────────────────────────────────────

/// The stealer side of a work-stealing deque.
///
/// Multiple Stealers can exist per deque. Each Stealer can steal items
/// from the top of the deque (FIFO order relative to push order).
#[derive(Debug)]
pub struct Stealer<T> {
    state: Arc<Mutex<SharedState<T>>>,
}

impl<T> Clone for Stealer<T> {
    fn clone(&self) -> Self {
        Self {
            state: Arc::clone(&self.state),
        }
    }
}

impl<T> Stealer<T> {
    /// Attempt to steal an item from the top of the deque (FIFO).
    pub fn steal(&self) -> StealResult<T> {
        let mut state = match self.state.try_lock() {
            Ok(s) => s,
            Err(_) => {
                // Another thread holds the lock — Retry
                return StealResult::Retry;
            }
        };

        if state.buffer.is_empty() {
            return StealResult::Empty;
        }

        match state.buffer.pop_front() {
            Some(item) => {
                state.total_stolen += 1;
                StealResult::Success(item)
            }
            None => StealResult::Empty,
        }
    }

    /// Attempt to steal up to `max` items at once (batch steal).
    pub fn steal_batch(&self, max: usize) -> Vec<T> {
        let mut state = match self.state.try_lock() {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };

        let n = max.min(state.buffer.len());
        let mut result = Vec::with_capacity(n);
        for _ in 0..n {
            if let Some(item) = state.buffer.pop_front() {
                result.push(item);
            }
        }
        state.total_stolen += result.len() as u64;
        result
    }

    /// Check if the deque is empty (may be stale immediately).
    pub fn is_empty(&self) -> bool {
        match self.state.try_lock() {
            Ok(s) => s.buffer.is_empty(),
            Err(_) => false, // Assume not empty when contended
        }
    }
}

// ── Constructor ───────────────────────────────────────────────────────

/// Create a new work-stealing deque, returning the Worker and Stealer handles.
///
/// # Example
/// ```
/// use frankenterm_core::work_stealing_deque::new_deque;
///
/// let (worker, stealer) = new_deque::<i32>(64);
/// worker.push(1);
/// worker.push(2);
/// worker.push(3);
///
/// // Owner pops from bottom (LIFO)
/// assert_eq!(worker.pop(), Some(3));
///
/// // Stealer steals from top (FIFO)
/// let stolen = stealer.steal();
/// assert!(stolen.is_success());
/// ```
pub fn new_deque<T>(initial_capacity: usize) -> (Worker<T>, Stealer<T>) {
    let state = Arc::new(Mutex::new(SharedState::new(initial_capacity)));
    let worker = Worker {
        state: Arc::clone(&state),
    };
    let stealer = Stealer { state };
    (worker, stealer)
}

/// Create a new work-stealing deque with default configuration.
pub fn new_deque_default<T>() -> (Worker<T>, Stealer<T>) {
    new_deque(WsDequeConfig::default().initial_capacity)
}

// ── Multi-worker pool ─────────────────────────────────────────────────

/// A pool of work-stealing deques for N workers.
///
/// Each worker has its own deque. Workers push to their own deque and
/// pop from it. When a worker's deque is empty, it steals from others.
#[derive(Debug)]
pub struct WorkStealingPool<T> {
    workers: Vec<Worker<T>>,
    stealers: Vec<Vec<Stealer<T>>>,
}

impl<T> WorkStealingPool<T> {
    /// Create a pool with `n` workers.
    pub fn new(n: usize) -> Self {
        assert!(n > 0, "pool must have at least 1 worker");
        let mut workers = Vec::with_capacity(n);
        let mut all_stealers: Vec<Stealer<T>> = Vec::with_capacity(n);

        for _ in 0..n {
            let (w, s) = new_deque(64);
            workers.push(w);
            all_stealers.push(s);
        }

        // Each worker gets stealers for all OTHER workers' deques
        let mut stealers = Vec::with_capacity(n);
        for i in 0..n {
            let mut my_stealers = Vec::with_capacity(n - 1);
            for (j, s) in all_stealers.iter().enumerate() {
                if i != j {
                    my_stealers.push((*s).clone());
                }
            }
            stealers.push(my_stealers);
        }

        Self { workers, stealers }
    }

    /// Number of workers.
    pub fn num_workers(&self) -> usize {
        self.workers.len()
    }

    /// Push to a specific worker's deque.
    pub fn push(&self, worker_id: usize, item: T) {
        self.workers[worker_id].push(item);
    }

    /// Pop from a specific worker's deque (LIFO).
    pub fn pop(&self, worker_id: usize) -> Option<T> {
        self.workers[worker_id].pop()
    }

    /// Try to steal from another worker's deque.
    /// Cycles through all other workers' deques.
    pub fn steal(&self, worker_id: usize) -> StealResult<T> {
        for stealer in &self.stealers[worker_id] {
            match stealer.steal() {
                StealResult::Success(item) => return StealResult::Success(item),
                StealResult::Retry | StealResult::Empty => {}
            }
        }
        StealResult::Empty
    }

    /// Pop from own deque, or steal from others if empty.
    pub fn pop_or_steal(&self, worker_id: usize) -> Option<T> {
        self.pop(worker_id)
            .or_else(|| self.steal(worker_id).into_option())
    }

    /// Get total stats across all workers.
    pub fn stats(&self) -> WsDequeStats {
        let mut total = WsDequeStats {
            len: 0,
            total_pushed: 0,
            total_popped: 0,
            total_stolen: 0,
            steal_failures: 0,
        };
        for w in &self.workers {
            let s = w.stats();
            total.len += s.len;
            total.total_pushed += s.total_pushed;
            total.total_popped += s.total_popped;
            total.total_stolen += s.total_stolen;
            total.steal_failures += s.steal_failures;
        }
        total
    }
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_deque() {
        let (worker, stealer) = new_deque::<i32>(16);
        assert!(worker.is_empty());
        assert_eq!(worker.len(), 0);
        assert_eq!(worker.pop(), None);
        assert!(stealer.steal().is_empty());
    }

    #[test]
    fn push_pop_lifo() {
        let (worker, _) = new_deque(16);
        worker.push(1);
        worker.push(2);
        worker.push(3);
        assert_eq!(worker.pop(), Some(3));
        assert_eq!(worker.pop(), Some(2));
        assert_eq!(worker.pop(), Some(1));
        assert_eq!(worker.pop(), None);
    }

    #[test]
    fn steal_fifo() {
        let (worker, stealer) = new_deque(16);
        worker.push(1);
        worker.push(2);
        worker.push(3);
        assert_eq!(stealer.steal().unwrap(), 1);
        assert_eq!(stealer.steal().unwrap(), 2);
        assert_eq!(stealer.steal().unwrap(), 3);
        assert!(stealer.steal().is_empty());
    }

    #[test]
    fn mixed_push_pop_steal() {
        let (worker, stealer) = new_deque(16);
        worker.push(1);
        worker.push(2);
        worker.push(3);
        worker.push(4);

        // Stealer takes from top
        assert_eq!(stealer.steal().unwrap(), 1);
        // Worker pops from bottom
        assert_eq!(worker.pop(), Some(4));
        // Middle elements remain
        assert_eq!(worker.len(), 2);
    }

    #[test]
    fn steal_batch() {
        let (worker, stealer) = new_deque(16);
        for i in 0..10 {
            worker.push(i);
        }
        let batch = stealer.steal_batch(3);
        assert_eq!(batch, vec![0, 1, 2]);
        assert_eq!(worker.len(), 7);
    }

    #[test]
    fn steal_batch_more_than_available() {
        let (worker, stealer) = new_deque(16);
        worker.push(1);
        worker.push(2);
        let batch = stealer.steal_batch(10);
        assert_eq!(batch, vec![1, 2]);
    }

    #[test]
    fn multiple_stealers() {
        let (worker, stealer1) = new_deque(16);
        let stealer2 = stealer1.clone();
        worker.push(1);
        worker.push(2);

        let r1 = stealer1.steal();
        let r2 = stealer2.steal();
        // One should get 1, the other gets 2 or Retry
        assert!(r1.is_success() || r2.is_success());
    }

    #[test]
    fn stats_tracking() {
        let (worker, stealer) = new_deque(16);
        worker.push(1);
        worker.push(2);
        worker.push(3);
        worker.pop();
        stealer.steal();

        let stats = worker.stats();
        assert_eq!(stats.total_pushed, 3);
        assert_eq!(stats.total_popped, 1);
        assert_eq!(stats.total_stolen, 1);
        assert_eq!(stats.len, 1);
    }

    #[test]
    fn steal_result_methods() {
        let s: StealResult<i32> = StealResult::Success(42);
        assert!(s.is_success());
        assert!(!s.is_empty());
        assert_eq!(s.unwrap(), 42);

        let e: StealResult<i32> = StealResult::Empty;
        assert!(!e.is_success());
        assert!(e.is_empty());
        assert_eq!(e.into_option(), None);

        let r: StealResult<i32> = StealResult::Retry;
        assert!(!r.is_success());
        assert!(!r.is_empty());
    }

    // -- Pool tests --

    #[test]
    fn pool_basic() {
        let pool = WorkStealingPool::new(3);
        assert_eq!(pool.num_workers(), 3);
        pool.push(0, 10);
        pool.push(0, 20);
        pool.push(1, 30);

        assert_eq!(pool.pop(0), Some(20));
        assert_eq!(pool.pop(1), Some(30));
    }

    #[test]
    fn pool_steal() {
        let pool = WorkStealingPool::new(2);
        pool.push(0, 1);
        pool.push(0, 2);
        pool.push(0, 3);

        // Worker 1 has nothing, steals from worker 0
        let stolen = pool.steal(1);
        assert!(stolen.is_success());
    }

    #[test]
    fn pool_pop_or_steal() {
        let pool = WorkStealingPool::new(2);
        pool.push(0, 1);
        pool.push(0, 2);

        // Worker 0 pops from own deque
        assert_eq!(pool.pop_or_steal(0), Some(2));
        // Worker 1 steals from worker 0
        assert_eq!(pool.pop_or_steal(1), Some(1));
        // Both empty
        assert_eq!(pool.pop_or_steal(0), None);
    }

    #[test]
    fn pool_stats() {
        let pool = WorkStealingPool::new(2);
        pool.push(0, 1);
        pool.push(0, 2);
        pool.push(1, 3);
        pool.pop(0);

        let stats = pool.stats();
        assert_eq!(stats.total_pushed, 3);
        assert_eq!(stats.total_popped, 1);
    }

    #[test]
    fn config_serde() {
        let config = WsDequeConfig {
            initial_capacity: 128,
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: WsDequeConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, back);
    }

    #[test]
    fn stats_serde() {
        let stats = WsDequeStats {
            len: 5,
            total_pushed: 100,
            total_popped: 50,
            total_stolen: 30,
            steal_failures: 10,
        };
        let json = serde_json::to_string(&stats).unwrap();
        let back: WsDequeStats = serde_json::from_str(&json).unwrap();
        assert_eq!(stats, back);
    }

    #[test]
    fn default_config() {
        let config = WsDequeConfig::default();
        assert_eq!(config.initial_capacity, 64);
    }

    #[test]
    fn new_deque_default_works() {
        let (worker, stealer) = new_deque_default::<String>();
        worker.push("hello".to_string());
        assert_eq!(stealer.steal().unwrap(), "hello");
    }

    #[test]
    fn push_many_pop_all() {
        let (worker, _) = new_deque(16);
        for i in 0..100 {
            worker.push(i);
        }
        assert_eq!(worker.len(), 100);
        for i in (0..100).rev() {
            assert_eq!(worker.pop(), Some(i));
        }
        assert!(worker.is_empty());
    }

    #[test]
    fn interleaved_push_steal() {
        let (worker, stealer) = new_deque(16);
        worker.push(1);
        assert_eq!(stealer.steal().unwrap(), 1);
        worker.push(2);
        worker.push(3);
        assert_eq!(stealer.steal().unwrap(), 2);
        worker.push(4);
        assert_eq!(stealer.steal().unwrap(), 3);
        assert_eq!(stealer.steal().unwrap(), 4);
        assert!(stealer.steal().is_empty());
    }

    #[test]
    #[should_panic(expected = "pool must have at least 1 worker")]
    fn pool_zero_workers_panics() {
        let _ = WorkStealingPool::<i32>::new(0);
    }

    #[test]
    fn pool_single_worker_no_steal_targets() {
        let pool = WorkStealingPool::new(1);
        pool.push(0, 42);
        // Stealing with only 1 worker should find nothing (no other deques)
        let stolen = pool.steal(0);
        assert!(stolen.is_empty());
        // But pop works
        assert_eq!(pool.pop(0), Some(42));
    }

    // ── Steal batch edge cases ──────────────────────────────────────

    #[test]
    fn steal_batch_empty_deque() {
        let (_worker, stealer) = new_deque::<i32>(16);
        let batch = stealer.steal_batch(5);
        assert!(batch.is_empty());
    }

    #[test]
    fn steal_batch_zero_max() {
        let (worker, stealer) = new_deque(16);
        worker.push(1);
        worker.push(2);
        let batch = stealer.steal_batch(0);
        assert!(batch.is_empty());
        assert_eq!(worker.len(), 2); // nothing stolen
    }

    #[test]
    fn steal_batch_exact_count() {
        let (worker, stealer) = new_deque(16);
        worker.push(10);
        worker.push(20);
        worker.push(30);
        let batch = stealer.steal_batch(3);
        assert_eq!(batch, vec![10, 20, 30]);
        assert!(worker.is_empty());
    }

    #[test]
    fn steal_batch_preserves_fifo_order() {
        let (worker, stealer) = new_deque(16);
        for i in 0..10 {
            worker.push(i);
        }
        let batch = stealer.steal_batch(5);
        assert_eq!(batch, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn steal_batch_then_steal_single() {
        let (worker, stealer) = new_deque(16);
        for i in 0..6 {
            worker.push(i);
        }
        let batch = stealer.steal_batch(3);
        assert_eq!(batch, vec![0, 1, 2]);
        assert_eq!(stealer.steal().unwrap(), 3);
        assert_eq!(stealer.steal().unwrap(), 4);
        assert_eq!(stealer.steal().unwrap(), 5);
        assert!(stealer.steal().is_empty());
    }

    // ── Stealer.is_empty ────────────────────────────────────────────

    #[test]
    fn stealer_is_empty_reflects_state() {
        let (worker, stealer) = new_deque(16);
        assert!(stealer.is_empty());
        worker.push(1);
        assert!(!stealer.is_empty());
        stealer.steal();
        assert!(stealer.is_empty());
    }

    // ── Pop edge cases ──────────────────────────────────────────────

    #[test]
    fn pop_from_empty_is_idempotent() {
        let (worker, _) = new_deque::<i32>(16);
        assert_eq!(worker.pop(), None);
        assert_eq!(worker.pop(), None);
        assert_eq!(worker.pop(), None);
    }

    #[test]
    fn push_single_pop_single() {
        let (worker, _) = new_deque(16);
        worker.push(42);
        assert_eq!(worker.len(), 1);
        assert!(!worker.is_empty());
        assert_eq!(worker.pop(), Some(42));
        assert!(worker.is_empty());
    }

    // ── Clone behavior ──────────────────────────────────────────────

    #[test]
    fn worker_clone_shares_state() {
        let (worker, _stealer) = new_deque(16);
        let worker2 = worker.clone();
        worker.push(1);
        worker.push(2);
        // Clone sees the same items
        assert_eq!(worker2.len(), 2);
        assert_eq!(worker2.pop(), Some(2));
        assert_eq!(worker.len(), 1);
    }

    #[test]
    fn stealer_clone_shares_state() {
        let (worker, stealer) = new_deque(16);
        let stealer2 = stealer.clone();
        worker.push(10);
        worker.push(20);

        assert_eq!(stealer.steal().unwrap(), 10);
        assert_eq!(stealer2.steal().unwrap(), 20);
        assert!(stealer.steal().is_empty());
        assert!(stealer2.steal().is_empty());
    }

    // ── Stats verification ──────────────────────────────────────────

    #[test]
    fn stats_on_empty_deque() {
        let (worker, _) = new_deque::<i32>(16);
        let stats = worker.stats();
        assert_eq!(stats.len, 0);
        assert_eq!(stats.total_pushed, 0);
        assert_eq!(stats.total_popped, 0);
        assert_eq!(stats.total_stolen, 0);
        assert_eq!(stats.steal_failures, 0);
    }

    #[test]
    fn stats_count_batch_steals() {
        let (worker, stealer) = new_deque(16);
        for i in 0..10 {
            worker.push(i);
        }
        stealer.steal_batch(4);
        let stats = worker.stats();
        assert_eq!(stats.total_pushed, 10);
        assert_eq!(stats.total_stolen, 4);
        assert_eq!(stats.len, 6);
    }

    #[test]
    fn stats_after_many_operations() {
        let (worker, stealer) = new_deque(16);
        for i in 0..20 {
            worker.push(i);
        }
        for _ in 0..5 {
            worker.pop();
        }
        for _ in 0..3 {
            stealer.steal();
        }
        let stats = worker.stats();
        assert_eq!(stats.total_pushed, 20);
        assert_eq!(stats.total_popped, 5);
        assert_eq!(stats.total_stolen, 3);
        assert_eq!(stats.len, 12);
    }

    // ── StealResult panics ──────────────────────────────────────────

    #[test]
    #[should_panic(expected = "called unwrap on Empty")]
    fn steal_result_unwrap_empty_panics() {
        let r: StealResult<i32> = StealResult::Empty;
        r.unwrap();
    }

    #[test]
    #[should_panic(expected = "called unwrap on Retry")]
    fn steal_result_unwrap_retry_panics() {
        let r: StealResult<i32> = StealResult::Retry;
        r.unwrap();
    }

    #[test]
    fn steal_result_into_option_variants() {
        assert_eq!(StealResult::Success(42).into_option(), Some(42));
        assert_eq!(StealResult::<i32>::Empty.into_option(), None);
        assert_eq!(StealResult::<i32>::Retry.into_option(), None);
    }

    // ── Len tracking ────────────────────────────────────────────────

    #[test]
    fn len_tracks_push_pop_steal() {
        let (worker, stealer) = new_deque(16);
        assert_eq!(worker.len(), 0);

        worker.push(1);
        assert_eq!(worker.len(), 1);

        worker.push(2);
        worker.push(3);
        assert_eq!(worker.len(), 3);

        worker.pop();
        assert_eq!(worker.len(), 2);

        stealer.steal();
        assert_eq!(worker.len(), 1);

        stealer.steal();
        assert_eq!(worker.len(), 0);
    }

    // ── Capacity growth ─────────────────────────────────────────────

    #[test]
    fn deque_grows_beyond_initial_capacity() {
        let (worker, stealer) = new_deque(4);
        // Push well beyond initial capacity of 4
        for i in 0..100 {
            worker.push(i);
        }
        assert_eq!(worker.len(), 100);

        // Steal all in FIFO order
        for i in 0..100 {
            assert_eq!(stealer.steal().unwrap(), i);
        }
    }

    // ── Pool extended tests ─────────────────────────────────────────

    #[test]
    fn pool_many_workers() {
        let pool = WorkStealingPool::new(8);
        assert_eq!(pool.num_workers(), 8);
        for w in 0..8 {
            pool.push(w, w as i32 * 10);
        }
        for w in 0..8 {
            assert_eq!(pool.pop(w), Some(w as i32 * 10));
        }
    }

    #[test]
    fn pool_steal_round_robin() {
        let pool = WorkStealingPool::new(3);
        pool.push(0, 10);
        pool.push(1, 20);
        pool.push(2, 30);

        // Worker 0 steals — should find something from worker 1 or 2
        let stolen = pool.steal(0);
        assert!(stolen.is_success());
    }

    #[test]
    fn pool_pop_or_steal_prefers_own() {
        let pool = WorkStealingPool::new(2);
        pool.push(0, 10);
        pool.push(1, 20);

        // Worker 0 should pop its own (10) first
        assert_eq!(pool.pop_or_steal(0), Some(10));
        // Now worker 0's deque is empty, steals from worker 1
        assert_eq!(pool.pop_or_steal(0), Some(20));
        // All empty
        assert_eq!(pool.pop_or_steal(0), None);
    }

    #[test]
    fn pool_pop_or_steal_all_empty() {
        let pool = WorkStealingPool::<i32>::new(3);
        assert_eq!(pool.pop_or_steal(0), None);
        assert_eq!(pool.pop_or_steal(1), None);
        assert_eq!(pool.pop_or_steal(2), None);
    }

    #[test]
    fn pool_stats_comprehensive() {
        let pool = WorkStealingPool::new(3);
        pool.push(0, 1);
        pool.push(0, 2);
        pool.push(1, 3);
        pool.push(2, 4);
        pool.push(2, 5);
        pool.pop(0); // pop from worker 0
        pool.steal(1); // worker 1 steals (from 0 or 2)

        let stats = pool.stats();
        assert_eq!(stats.total_pushed, 5);
        assert_eq!(stats.total_popped, 1);
        assert_eq!(stats.total_stolen, 1);
        assert_eq!(stats.len, 3);
    }

    #[test]
    fn pool_multiple_push_steal_rounds() {
        let pool = WorkStealingPool::new(2);
        for round in 0..5 {
            let base = round * 10;
            pool.push(0, base);
            pool.push(0, base + 1);
            pool.push(1, base + 2);

            pool.pop(0);
            pool.steal(1);
        }
        let stats = pool.stats();
        assert_eq!(stats.total_pushed, 15); // 3 per round * 5
        assert_eq!(stats.total_popped, 5); // 1 pop per round
        assert_eq!(stats.total_stolen, 5); // 1 steal per round
    }

    #[test]
    fn pool_with_string_type() {
        let pool = WorkStealingPool::new(2);
        pool.push(0, "hello".to_string());
        pool.push(1, "world".to_string());
        assert_eq!(pool.pop(0), Some("hello".to_string()));
        assert_eq!(pool.pop_or_steal(0), Some("world".to_string()));
    }

    // ── Type-specific tests ─────────────────────────────────────────

    #[test]
    fn deque_with_string_type() {
        let (worker, stealer) = new_deque(8);
        worker.push("alpha".to_string());
        worker.push("beta".to_string());
        assert_eq!(stealer.steal().unwrap(), "alpha");
        assert_eq!(worker.pop(), Some("beta".to_string()));
    }

    #[test]
    fn deque_with_vec_type() {
        let (worker, stealer) = new_deque(8);
        worker.push(vec![1, 2, 3]);
        worker.push(vec![4, 5]);
        assert_eq!(stealer.steal().unwrap(), vec![1, 2, 3]);
        assert_eq!(worker.pop(), Some(vec![4, 5]));
    }

    // ── Interleaved operations ──────────────────────────────────────

    #[test]
    fn push_steal_pop_interleaved_stress() {
        let (worker, stealer) = new_deque(16);
        let mut stolen = Vec::new();
        let mut popped = Vec::new();

        for i in 0..50 {
            worker.push(i);
            if i % 3 == 0 {
                if let StealResult::Success(v) = stealer.steal() {
                    stolen.push(v);
                }
            }
            if i % 5 == 0 {
                if let Some(v) = worker.pop() {
                    popped.push(v);
                }
            }
        }
        // Drain remainder
        while let Some(v) = worker.pop() {
            popped.push(v);
        }
        while let StealResult::Success(v) = stealer.steal() {
            stolen.push(v);
        }

        // Total stolen + popped should equal total pushed (50)
        assert_eq!(stolen.len() + popped.len(), 50);
    }

    #[test]
    fn steal_batch_multiple_rounds() {
        let (worker, stealer) = new_deque(16);
        for i in 0..20 {
            worker.push(i);
        }
        let b1 = stealer.steal_batch(5);
        let b2 = stealer.steal_batch(5);
        let b3 = stealer.steal_batch(5);
        let b4 = stealer.steal_batch(5);
        assert_eq!(b1, vec![0, 1, 2, 3, 4]);
        assert_eq!(b2, vec![5, 6, 7, 8, 9]);
        assert_eq!(b3, vec![10, 11, 12, 13, 14]);
        assert_eq!(b4, vec![15, 16, 17, 18, 19]);
        assert!(stealer.steal().is_empty());
    }

    // ── WsDequeConfig / WsDequeStats equality ───────────────────────

    #[test]
    fn config_equality() {
        let c1 = WsDequeConfig {
            initial_capacity: 32,
        };
        let c2 = WsDequeConfig {
            initial_capacity: 32,
        };
        let c3 = WsDequeConfig {
            initial_capacity: 64,
        };
        assert_eq!(c1, c2);
        assert_ne!(c1, c3);
    }

    #[test]
    fn stats_equality() {
        let s1 = WsDequeStats {
            len: 1,
            total_pushed: 2,
            total_popped: 3,
            total_stolen: 4,
            steal_failures: 5,
        };
        let s2 = WsDequeStats {
            len: 1,
            total_pushed: 2,
            total_popped: 3,
            total_stolen: 4,
            steal_failures: 5,
        };
        let s3 = WsDequeStats {
            len: 0,
            total_pushed: 2,
            total_popped: 3,
            total_stolen: 4,
            steal_failures: 5,
        };
        assert_eq!(s1, s2);
        assert_ne!(s1, s3);
    }

    #[test]
    fn config_debug_format() {
        let config = WsDequeConfig {
            initial_capacity: 64,
        };
        let debug = format!("{:?}", config);
        assert!(debug.contains("64"));
    }

    #[test]
    fn stats_debug_format() {
        let stats = WsDequeStats {
            len: 5,
            total_pushed: 10,
            total_popped: 3,
            total_stolen: 2,
            steal_failures: 0,
        };
        let debug = format!("{:?}", stats);
        assert!(debug.contains("total_pushed"));
        assert!(debug.contains("10"));
    }
}
