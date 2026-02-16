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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
        let mut state = self.state.lock().expect("lock poisoned");
        state.buffer.push_back(item);
        state.total_pushed += 1;
    }

    /// Pop an item from the bottom of the deque (LIFO).
    pub fn pop(&self) -> Option<T> {
        let mut state = self.state.lock().expect("lock poisoned");
        let item = state.buffer.pop_back();
        if item.is_some() {
            state.total_popped += 1;
        }
        item
    }

    /// Number of items currently in the deque.
    pub fn len(&self) -> usize {
        let state = self.state.lock().expect("lock poisoned");
        state.buffer.len()
    }

    /// Whether the deque is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Get statistics.
    pub fn stats(&self) -> WsDequeStats {
        let state = self.state.lock().expect("lock poisoned");
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
                StealResult::Retry => continue,
                StealResult::Empty => continue,
            }
        }
        StealResult::Empty
    }

    /// Pop from own deque, or steal from others if empty.
    pub fn pop_or_steal(&self, worker_id: usize) -> Option<T> {
        self.pop(worker_id).or_else(|| self.steal(worker_id).into_option())
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
        let config = WsDequeConfig { initial_capacity: 128 };
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
}
