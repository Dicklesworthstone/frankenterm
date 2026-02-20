//! Property-based tests for the runtime_compat dual-runtime abstraction layer.
//!
//! Verifies critical invariants of the tokio↔asupersync migration bridge:
//! - MPSC channel FIFO ordering: messages arrive in send order
//! - MPSC channel completeness: all sent messages are received
//! - MPSC helpers roundtrip: mpsc_send + mpsc_recv_option preserve values
//! - Watch channel latest-value: borrow always returns the most recent send
//! - Watch channel convergence: multiple sends → receiver sees final value
//! - Broadcast channel FIFO: receivers get messages in order
//! - Broadcast channel fan-out: all receivers get same messages
//! - Semaphore permit accounting: acquire/release preserves total permits
//! - Semaphore monotonic drain: each acquire reduces available by 1
//! - Mutex data integrity: values survive lock/unlock cycles
//! - Mutex sequential consistency: last write wins
//! - RwLock data integrity: write then read returns written value
//! - RwLock read stability: concurrent reads see same value
//! - RuntimeBuilder builder pattern: always produces valid runtime
//! - RuntimeBuilder worker_threads chainable: any thread count accepted
//! - Timeout fast-path: immediate futures never timeout
//! - Timeout returns correct value type
//! - spawn_blocking roundtrip: closure result preserved
//! - spawn_blocking with heavy computation: values correct
//! - Sleep non-negative: always completes (never panics)

use proptest::prelude::*;
use std::sync::Arc;
use std::time::Duration;

use frankenterm_core::runtime_compat::{
    self, CompatRuntime, Mutex, RuntimeBuilder, RwLock, Semaphore, broadcast, mpsc, notify, watch,
};

// ────────────────────────────────────────────────────────────────────
// Strategies
// ────────────────────────────────────────────────────────────────────

fn arb_message_sequence() -> impl Strategy<Value = Vec<i64>> {
    prop::collection::vec(any::<i64>(), 0..50)
}

fn arb_small_message_sequence() -> impl Strategy<Value = Vec<u32>> {
    prop::collection::vec(0u32..1000, 1..20)
}

fn arb_string_sequence() -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec("[a-z]{1,10}", 1..15)
}

fn arb_channel_capacity() -> impl Strategy<Value = usize> {
    1usize..=100
}

fn arb_semaphore_permits() -> impl Strategy<Value = usize> {
    1usize..=50
}

fn arb_worker_threads() -> impl Strategy<Value = usize> {
    1usize..=8
}

fn arb_timeout_ms() -> impl Strategy<Value = u64> {
    100u64..=5000
}

fn arb_timeout_work_ms() -> impl Strategy<Value = u64> {
    10u64..=40
}

fn arb_short_timeout_ms() -> impl Strategy<Value = u64> {
    0u64..=2
}

fn arb_long_timeout_padding_ms() -> impl Strategy<Value = u64> {
    80u64..=160
}

fn arb_sleep_ms() -> impl Strategy<Value = u64> {
    0u64..=50
}

fn arb_numeric_value() -> impl Strategy<Value = i64> {
    any::<i64>()
}

fn arb_write_sequence() -> impl Strategy<Value = Vec<i32>> {
    prop::collection::vec(any::<i32>(), 1..20)
}

// ────────────────────────────────────────────────────────────────────
// Helper: run async property inside tokio runtime
// ────────────────────────────────────────────────────────────────────

fn with_tokio<F, Fut>(f: F)
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let rt = RuntimeBuilder::current_thread()
        .build()
        .expect("tokio runtime");
    rt.block_on(f());
}

// ────────────────────────────────────────────────────────────────────
// MPSC Channel Properties
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// All messages sent through mpsc arrive in FIFO order.
    #[test]
    fn mpsc_fifo_ordering(
        msgs in arb_message_sequence(),
        cap in arb_channel_capacity(),
    ) {
        let cap = cap.max(msgs.len().max(1));
        let msgs_clone = msgs.clone();
        with_tokio(move || async move {
            let (tx, mut rx) = mpsc::channel(cap);
            for m in &msgs_clone {
                runtime_compat::mpsc_send(&tx, *m).await.expect("send");
            }
            drop(tx);
            let mut received = Vec::new();
            while let Some(v) = runtime_compat::mpsc_recv_option(&mut rx).await {
                received.push(v);
            }
            assert_eq!(received, msgs_clone, "MPSC must preserve FIFO order");
        });
    }

    /// mpsc channel delivers exactly as many messages as were sent.
    #[test]
    fn mpsc_completeness(
        msgs in arb_small_message_sequence(),
        cap in arb_channel_capacity(),
    ) {
        let cap = cap.max(msgs.len().max(1));
        let count = msgs.len();
        with_tokio(move || async move {
            let (tx, mut rx) = mpsc::channel(cap);
            for m in msgs {
                runtime_compat::mpsc_send(&tx, m).await.expect("send");
            }
            drop(tx);
            let mut recv_count = 0usize;
            while runtime_compat::mpsc_recv_option(&mut rx).await.is_some() {
                recv_count += 1;
            }
            assert_eq!(recv_count, count, "received count must equal sent count");
        });
    }

    /// mpsc_send to a closed receiver returns Err containing the original value.
    #[test]
    fn mpsc_send_to_closed_returns_value(val in arb_numeric_value()) {
        with_tokio(move || async move {
            let (tx, rx) = mpsc::channel::<i64>(1);
            drop(rx);
            let err = runtime_compat::mpsc_send(&tx, val).await;
            assert!(err.is_err(), "send to closed channel must fail");
            let send_err = err.unwrap_err();

            #[cfg(feature = "asupersync-runtime")]
            assert!(
                matches!(
                    send_err,
                    mpsc::SendError::Disconnected(value) if value == val
                ),
                "SendError must contain the original value",
            );

            #[cfg(not(feature = "asupersync-runtime"))]
            assert_eq!(send_err.0, val, "SendError must contain the original value");
        });
    }

    /// mpsc_recv_option returns None when sender is dropped and channel drained.
    #[test]
    fn mpsc_recv_none_after_close(
        msgs in arb_small_message_sequence(),
    ) {
        with_tokio(move || async move {
            let (tx, mut rx) = mpsc::channel(msgs.len().max(1));
            for m in &msgs {
                runtime_compat::mpsc_send(&tx, *m).await.expect("send");
            }
            drop(tx);
            // Drain all messages
            for _ in &msgs {
                let _ = runtime_compat::mpsc_recv_option(&mut rx).await;
            }
            // Next recv must be None
            let last = runtime_compat::mpsc_recv_option(&mut rx).await;
            assert_eq!(last, None, "recv after drain+close must be None");
        });
    }

    /// String values survive mpsc roundtrip.
    #[test]
    fn mpsc_string_roundtrip(
        msgs in arb_string_sequence(),
    ) {
        let msgs_clone = msgs.clone();
        with_tokio(move || async move {
            let (tx, mut rx) = mpsc::channel(msgs_clone.len().max(1));
            for m in &msgs_clone {
                runtime_compat::mpsc_send(&tx, m.clone()).await.expect("send");
            }
            drop(tx);
            let mut received = Vec::new();
            while let Some(v) = runtime_compat::mpsc_recv_option(&mut rx).await {
                received.push(v);
            }
            assert_eq!(received, msgs_clone, "strings must survive mpsc roundtrip");
        });
    }
}

// ────────────────────────────────────────────────────────────────────
// Watch Channel Properties
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Watch channel borrow always returns the most recently sent value.
    #[test]
    fn watch_latest_value(
        values in arb_write_sequence(),
    ) {
        prop_assume!(!values.is_empty());
        with_tokio(move || async move {
            let (tx, rx) = watch::channel(0i32);
            let expected = *values.last().unwrap();
            for v in &values {
                tx.send(*v).expect("watch send");
            }
            let observed = *rx.borrow();
            assert_eq!(observed, expected, "watch borrow must return latest value");
        });
    }

    /// Watch channel with initial value: borrow before any send returns init.
    #[test]
    fn watch_initial_value_preserved(init in any::<i64>()) {
        with_tokio(move || async move {
            let (_, rx) = watch::channel(init);
            assert_eq!(*rx.borrow(), init, "initial value must be preserved");
        });
    }

    /// Cloned watch receivers see the same value.
    #[test]
    fn watch_cloned_receivers_converge(
        values in arb_write_sequence(),
    ) {
        prop_assume!(!values.is_empty());
        with_tokio(move || async move {
            let (tx, rx1) = watch::channel(0i32);
            let rx2 = rx1.clone();
            for v in &values {
                tx.send(*v).expect("send");
            }
            assert_eq!(*rx1.borrow(), *rx2.borrow(), "cloned receivers must converge");
        });
    }

    /// Watch send after all receivers dropped returns Err.
    #[test]
    fn watch_send_to_no_receivers_fails(val in any::<i32>()) {
        with_tokio(move || async move {
            let (tx, rx) = watch::channel(0i32);
            drop(rx);
            let result = tx.send(val);
            assert!(result.is_err(), "send with no receivers must fail");
        });
    }
}

// ────────────────────────────────────────────────────────────────────
// Broadcast Channel Properties
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(150))]

    /// Broadcast receivers get messages in FIFO order.
    #[test]
    fn broadcast_fifo_ordering(
        msgs in prop::collection::vec(any::<i32>(), 1..20),
    ) {
        let cap = msgs.len() + 1;
        let msgs_clone = msgs.clone();
        with_tokio(move || async move {
            let (tx, mut rx) = broadcast::channel(cap);
            for m in &msgs_clone {
                tx.send(*m).expect("broadcast send");
            }
            let mut received = Vec::new();
            for _ in &msgs_clone {
                received.push(rx.recv().await.expect("broadcast recv"));
            }
            assert_eq!(received, msgs_clone, "broadcast must preserve FIFO order");
        });
    }

    /// Multiple broadcast receivers all see the same messages.
    #[test]
    fn broadcast_fanout_consistency(
        msgs in prop::collection::vec(any::<i32>(), 1..10),
    ) {
        let cap = msgs.len() + 1;
        let msgs_clone = msgs.clone();
        with_tokio(move || async move {
            let (tx, mut rx1) = broadcast::channel(cap);
            let mut rx2 = tx.subscribe();
            let mut rx3 = tx.subscribe();
            for m in &msgs_clone {
                tx.send(*m).expect("broadcast send");
            }
            let mut r1 = Vec::new();
            let mut r2 = Vec::new();
            let mut r3 = Vec::new();
            for _ in &msgs_clone {
                r1.push(rx1.recv().await.expect("rx1 recv"));
                r2.push(rx2.recv().await.expect("rx2 recv"));
                r3.push(rx3.recv().await.expect("rx3 recv"));
            }
            assert_eq!(r1, msgs_clone, "rx1 must match sent messages");
            assert_eq!(r2, msgs_clone, "rx2 must match sent messages");
            assert_eq!(r3, msgs_clone, "rx3 must match sent messages");
        });
    }

    /// Broadcast send with no receivers returns error.
    #[test]
    fn broadcast_no_receivers_error(val in any::<i32>()) {
        with_tokio(move || async move {
            let (tx, rx) = broadcast::channel::<i32>(16);
            drop(rx);
            let result = tx.send(val);
            assert!(result.is_err(), "broadcast send with no receivers must fail");
        });
    }
}

// ────────────────────────────────────────────────────────────────────
// Semaphore Permit Accounting Properties
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Semaphore initial permits match constructor argument.
    #[test]
    fn semaphore_initial_permits(n in arb_semaphore_permits()) {
        with_tokio(move || async move {
            let sem = Semaphore::new(n);
            assert_eq!(sem.available_permits(), n, "initial permits must match constructor");
        });
    }

    /// Each try_acquire reduces available permits by exactly 1.
    #[test]
    fn semaphore_try_acquire_decrements_by_one(
        total in arb_semaphore_permits(),
        acquire_count in 1usize..=50,
    ) {
        let acquire_count = acquire_count.min(total);
        with_tokio(move || async move {
            let sem = Semaphore::new(total);
            let mut permits = Vec::new();
            for i in 0..acquire_count {
                let p = sem.try_acquire();
                assert!(p.is_ok(), "try_acquire {} of {} must succeed", i + 1, total);
                permits.push(p.unwrap());
                assert_eq!(
                    sem.available_permits(),
                    total - i - 1,
                    "available must decrease by 1 per acquire"
                );
            }
        });
    }

    /// Dropping all permits restores original count.
    #[test]
    fn semaphore_release_restores_permits(
        total in arb_semaphore_permits(),
        acquire_count in 1usize..=50,
    ) {
        let acquire_count = acquire_count.min(total);
        with_tokio(move || async move {
            let sem = Semaphore::new(total);
            let mut permits = Vec::new();
            for _ in 0..acquire_count {
                permits.push(sem.try_acquire().expect("acquire"));
            }
            assert_eq!(sem.available_permits(), total - acquire_count);
            drop(permits);
            assert_eq!(sem.available_permits(), total, "all permits restored after drop");
        });
    }

    /// try_acquire after exhaustion returns NoPermits error.
    #[test]
    fn semaphore_exhausted_returns_no_permits(total in arb_semaphore_permits()) {
        with_tokio(move || async move {
            let sem = Semaphore::new(total);
            let mut held = Vec::new();
            for _ in 0..total {
                held.push(sem.try_acquire().expect("acquire"));
            }
            let err = sem.try_acquire();
            assert!(err.is_err(), "try_acquire after exhaustion must fail");
            drop(held);
        });
    }

    /// Closed semaphore rejects all try_acquire.
    #[test]
    fn semaphore_closed_rejects_try_acquire(total in arb_semaphore_permits()) {
        with_tokio(move || async move {
            let sem = Semaphore::new(total);
            sem.close();
            let result = sem.try_acquire();
            assert!(result.is_err(), "closed semaphore must reject try_acquire");
        });
    }

    /// Partial acquire then close: held permits survive, new acquires fail.
    #[test]
    fn semaphore_close_preserves_held(
        total in 2usize..=20,
        hold_count in 1usize..=19,
    ) {
        let hold_count = hold_count.min(total - 1);
        with_tokio(move || async move {
            let sem = Semaphore::new(total);
            let mut held = Vec::new();
            for _ in 0..hold_count {
                held.push(sem.try_acquire().expect("acquire before close"));
            }
            sem.close();
            let err = sem.try_acquire();
            assert!(err.is_err(), "try_acquire after close must fail");
            // Held permits are still valid (no panic on drop)
            drop(held);
        });
    }
}

// ────────────────────────────────────────────────────────────────────
// Mutex Data Integrity Properties
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Mutex preserves the last written value.
    #[test]
    fn mutex_last_write_wins(
        writes in arb_write_sequence(),
    ) {
        prop_assume!(!writes.is_empty());
        let expected = *writes.last().unwrap();
        with_tokio(move || async move {
            let m = Mutex::new(0i32);
            for w in &writes {
                let mut guard = m.lock().await;
                *guard = *w;
            }
            let guard = m.lock().await;
            assert_eq!(*guard, expected, "mutex must reflect last write");
        });
    }

    /// Mutex accumulation: summing a sequence produces correct total.
    #[test]
    fn mutex_accumulation_correct(
        values in prop::collection::vec(1i64..100, 1..30),
    ) {
        let expected: i64 = values.iter().sum();
        with_tokio(move || async move {
            let m = Mutex::new(0i64);
            for v in &values {
                let mut guard = m.lock().await;
                *guard += v;
            }
            let guard = m.lock().await;
            assert_eq!(*guard, expected, "mutex accumulation must be correct");
        });
    }

    /// Mutex with Vec: push then read preserves all elements.
    #[test]
    fn mutex_vec_integrity(
        items in prop::collection::vec(any::<u16>(), 0..30),
    ) {
        let items_clone = items.clone();
        with_tokio(move || async move {
            let m = Mutex::new(Vec::new());
            for item in &items_clone {
                let mut guard = m.lock().await;
                guard.push(*item);
            }
            let guard = m.lock().await;
            assert_eq!(*guard, items_clone, "mutex Vec must preserve all pushed items");
        });
    }

    /// Mutex initial value preserved when no writes occur.
    #[test]
    fn mutex_initial_value_preserved(init in any::<i64>()) {
        with_tokio(move || async move {
            let m = Mutex::new(init);
            let guard = m.lock().await;
            assert_eq!(*guard, init, "initial value must be preserved with no writes");
        });
    }
}

// ────────────────────────────────────────────────────────────────────
// RwLock Data Integrity Properties
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// RwLock write then read returns the written value.
    #[test]
    fn rwlock_write_then_read(val in any::<i64>()) {
        with_tokio(move || async move {
            let rw = RwLock::new(0i64);
            {
                let mut guard = rw.write().await;
                *guard = val;
            }
            let guard = rw.read().await;
            assert_eq!(*guard, val, "read after write must return written value");
        });
    }

    /// RwLock last write wins across a sequence.
    #[test]
    fn rwlock_last_write_wins(
        writes in arb_write_sequence(),
    ) {
        prop_assume!(!writes.is_empty());
        let expected = *writes.last().unwrap();
        with_tokio(move || async move {
            let rw = RwLock::new(0i32);
            for w in &writes {
                let mut guard = rw.write().await;
                *guard = *w;
            }
            let guard = rw.read().await;
            assert_eq!(*guard, expected, "rwlock must reflect last write");
        });
    }

    /// RwLock accumulation via write preserves sum.
    #[test]
    fn rwlock_accumulation(
        values in prop::collection::vec(1i64..100, 1..30),
    ) {
        let expected: i64 = values.iter().sum();
        with_tokio(move || async move {
            let rw = RwLock::new(0i64);
            for v in &values {
                let mut guard = rw.write().await;
                *guard += v;
            }
            let guard = rw.read().await;
            assert_eq!(*guard, expected, "rwlock accumulation must be correct");
        });
    }

    /// RwLock initial value preserved with read-only access.
    #[test]
    fn rwlock_initial_value_preserved(init in any::<i64>()) {
        with_tokio(move || async move {
            let rw = RwLock::new(init);
            let g1 = rw.read().await;
            assert_eq!(*g1, init);
            drop(g1);
            let g2 = rw.read().await;
            assert_eq!(*g2, init, "multiple reads must return same initial value");
        });
    }

    /// RwLock Vec integrity: write elements then verify.
    #[test]
    fn rwlock_vec_integrity(
        items in prop::collection::vec(any::<u8>(), 0..30),
    ) {
        let items_clone = items.clone();
        with_tokio(move || async move {
            let rw = RwLock::new(Vec::new());
            {
                let mut guard = rw.write().await;
                for item in &items_clone {
                    guard.push(*item);
                }
            }
            let guard = rw.read().await;
            assert_eq!(*guard, items_clone, "rwlock Vec must preserve all pushed items");
        });
    }
}

// ────────────────────────────────────────────────────────────────────
// RuntimeBuilder Properties
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// RuntimeBuilder::current_thread always builds successfully.
    #[test]
    fn runtime_builder_current_thread_always_succeeds(_dummy in 0u8..1) {
        let result = RuntimeBuilder::current_thread().build();
        prop_assert!(result.is_ok(), "current_thread build must always succeed");
    }

    /// RuntimeBuilder::multi_thread always builds successfully.
    #[test]
    fn runtime_builder_multi_thread_always_succeeds(_dummy in 0u8..1) {
        let result = RuntimeBuilder::multi_thread().build();
        prop_assert!(result.is_ok(), "multi_thread build must always succeed");
    }

    /// RuntimeBuilder::multi_thread with any worker_threads builds successfully.
    #[test]
    fn runtime_builder_worker_threads_valid(n in arb_worker_threads()) {
        let result = RuntimeBuilder::multi_thread().worker_threads(n).build();
        prop_assert!(result.is_ok(), "multi_thread with {} workers must succeed", n);
    }

    /// RuntimeBuilder::current_thread silently ignores worker_threads.
    #[test]
    fn runtime_builder_current_thread_ignores_workers(n in arb_worker_threads()) {
        let result = RuntimeBuilder::current_thread().worker_threads(n).build();
        prop_assert!(result.is_ok(), "current_thread must silently ignore worker_threads({})", n);
    }

    /// block_on preserves arbitrary computation results.
    #[test]
    fn block_on_preserves_value(val in any::<i64>()) {
        let rt = RuntimeBuilder::current_thread().build().expect("build");
        let result = rt.block_on(async move { val });
        prop_assert_eq!(result, val, "block_on must preserve computation result");
    }

    /// block_on with nested async preserves value.
    #[test]
    fn block_on_nested_async(a in any::<i32>(), b in any::<i32>()) {
        let rt = RuntimeBuilder::current_thread().build().expect("build");
        let result = rt.block_on(async move {
            let x = async move { a }.await;
            let y = async move { b }.await;
            (x, y)
        });
        prop_assert_eq!(result, (a, b), "nested async must preserve both values");
    }

    /// spawn_detached eventually runs and can signal completion through mpsc.
    #[test]
    fn spawn_detached_completes_and_signals(val in any::<i64>()) {
        let rt = RuntimeBuilder::multi_thread()
            .worker_threads(1)
            .build()
            .expect("build");
        let (tx, mut rx) = mpsc::channel(1);

        rt.spawn_detached(async move {
            runtime_compat::mpsc_send(&tx, val)
                .await
                .expect("spawn_detached send");
        });

        let observed = rt.block_on(async {
            runtime_compat::timeout(
                Duration::from_secs(1),
                runtime_compat::mpsc_recv_option(&mut rx),
            )
            .await
            .expect("spawn_detached must signal")
        });

        prop_assert_eq!(observed, Some(val), "spawn_detached must preserve payload");
    }
}

// ────────────────────────────────────────────────────────────────────
// Timeout Properties
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Immediate futures never timeout regardless of timeout duration.
    #[test]
    fn timeout_immediate_future_never_expires(
        val in any::<i64>(),
        timeout_ms in arb_timeout_ms(),
    ) {
        with_tokio(move || async move {
            let dur = Duration::from_millis(timeout_ms);
            let result = runtime_compat::timeout(dur, async move { val }).await;
            assert!(result.is_ok(), "immediate future must not timeout");
            assert_eq!(result.unwrap(), val, "timeout must preserve return value");
        });
    }

    /// Timeout preserves complex return types.
    #[test]
    fn timeout_preserves_complex_type(
        items in prop::collection::vec(any::<u8>(), 0..20),
    ) {
        let items_clone = items.clone();
        with_tokio(move || async move {
            let result = runtime_compat::timeout(
                Duration::from_secs(5),
                async move { items_clone },
            )
            .await;
            assert!(result.is_ok());
            assert_eq!(result.unwrap(), items, "timeout must preserve Vec values");
        });
    }

    /// Timeout preserves Result<T, E> types.
    #[test]
    fn timeout_preserves_result_type(val in any::<i32>()) {
        with_tokio(move || async move {
            let result = runtime_compat::timeout(
                Duration::from_secs(5),
                async move { Ok::<_, String>(val) },
            )
            .await;
            assert!(result.is_ok());
            assert_eq!(result.unwrap().unwrap(), val);
        });
    }

    /// Timeout behavior is monotonic for the same bounded task:
    /// a very short timeout fails, while a sufficiently longer timeout succeeds.
    #[test]
    fn timeout_deadline_monotonicity(
        work_ms in arb_timeout_work_ms(),
        short_timeout_ms in arb_short_timeout_ms(),
        long_timeout_padding_ms in arb_long_timeout_padding_ms(),
    ) {
        with_tokio(move || async move {
            let short = runtime_compat::timeout(
                Duration::from_millis(short_timeout_ms),
                async move {
                    runtime_compat::sleep(Duration::from_millis(work_ms)).await;
                    1u8
                },
            )
            .await;

            let long = runtime_compat::timeout(
                Duration::from_millis(work_ms + long_timeout_padding_ms),
                async move {
                    runtime_compat::sleep(Duration::from_millis(work_ms)).await;
                    1u8
                },
            )
            .await;

            assert!(short.is_err(), "very short timeout must expire");
            assert_eq!(long, Ok(1u8), "long timeout should allow completion");
        });
    }
}

// ────────────────────────────────────────────────────────────────────
// spawn_blocking Properties
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// spawn_blocking returns the closure's result.
    #[test]
    fn spawn_blocking_returns_closure_result(val in any::<i64>()) {
        with_tokio(move || async move {
            let result = runtime_compat::spawn_blocking(move || val).await;
            assert!(result.is_ok(), "spawn_blocking must succeed");
            assert_eq!(result.unwrap(), val, "spawn_blocking must return closure result");
        });
    }

    /// spawn_blocking with computation preserves correctness.
    #[test]
    fn spawn_blocking_computation(a in 0i64..1000, b in 0i64..1000) {
        let expected = a * b + a + b;
        with_tokio(move || async move {
            let result = runtime_compat::spawn_blocking(move || {
                a * b + a + b
            })
            .await;
            assert!(result.is_ok());
            assert_eq!(result.unwrap(), expected, "spawn_blocking computation must be correct");
        });
    }

    /// spawn_blocking preserves String values.
    #[test]
    fn spawn_blocking_string_roundtrip(s in "[a-zA-Z0-9]{0,50}") {
        let s_clone = s.clone();
        with_tokio(move || async move {
            let result = runtime_compat::spawn_blocking(move || s_clone).await;
            assert!(result.is_ok());
            assert_eq!(result.unwrap(), s, "spawn_blocking must preserve String");
        });
    }

    /// spawn_blocking with Vec construction preserves all elements.
    #[test]
    fn spawn_blocking_vec_construction(
        items in prop::collection::vec(any::<u16>(), 0..50),
    ) {
        let items_clone = items.clone();
        with_tokio(move || async move {
            let result = runtime_compat::spawn_blocking(move || {
                items_clone.into_iter().collect::<Vec<_>>()
            })
            .await;
            assert!(result.is_ok());
            assert_eq!(result.unwrap(), items, "spawn_blocking Vec must preserve all elements");
        });
    }
}

// ────────────────────────────────────────────────────────────────────
// Sleep Properties
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// Sleep with any small duration completes without panic.
    #[test]
    fn sleep_always_completes(ms in arb_sleep_ms()) {
        with_tokio(move || async move {
            runtime_compat::sleep(Duration::from_millis(ms)).await;
            // If we get here, sleep completed without panic
        });
    }

    /// Sleep with zero duration completes almost instantly.
    #[test]
    fn sleep_zero_instant(_dummy in 0u8..1) {
        with_tokio(|| async {
            let start = std::time::Instant::now();
            runtime_compat::sleep(Duration::ZERO).await;
            assert!(
                start.elapsed() < Duration::from_millis(100),
                "zero-duration sleep must complete quickly"
            );
        });
    }
}

// ────────────────────────────────────────────────────────────────────
// Cross-primitive Integration Properties
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Mutex + mpsc integration: protected counter matches message count.
    #[test]
    fn mutex_mpsc_integration(
        msgs in prop::collection::vec(1u32..100, 1..20),
    ) {
        let msg_count = msgs.len();
        let expected_sum: u32 = msgs.iter().sum();
        with_tokio(move || async move {
            let counter = Mutex::new(0u32);
            let (tx, mut rx) = mpsc::channel(msg_count.max(1));
            // Send all messages
            for m in &msgs {
                runtime_compat::mpsc_send(&tx, *m).await.expect("send");
            }
            drop(tx);
            // Receive and accumulate under mutex
            while let Some(v) = runtime_compat::mpsc_recv_option(&mut rx).await {
                let mut guard = counter.lock().await;
                *guard += v;
            }
            let total = *counter.lock().await;
            assert_eq!(total, expected_sum, "mutex+mpsc accumulation must match sum");
        });
    }

    /// Semaphore + Mutex: bounded-access counter.
    #[test]
    fn semaphore_mutex_bounded_access(
        permits in 1usize..=5,
        iterations in 1usize..=20,
    ) {
        with_tokio(move || async move {
            let sem = Semaphore::new(permits);
            let counter = Mutex::new(0usize);
            for _ in 0..iterations {
                let permit = sem.acquire().await.expect("acquire");
                {
                    let mut guard = counter.lock().await;
                    *guard += 1;
                }
                drop(permit);
            }
            let final_count = *counter.lock().await;
            assert_eq!(final_count, iterations, "bounded counter must equal iterations");
            assert_eq!(
                sem.available_permits(), permits,
                "all permits restored after all iterations"
            );
        });
    }

    /// block_on + Mutex + mpsc: runtime hosts channel+mutex workflow.
    #[test]
    fn runtime_hosts_channel_mutex_workflow(
        vals in prop::collection::vec(any::<i32>(), 1..10),
    ) {
        let vals_clone = vals.clone();
        let rt = RuntimeBuilder::current_thread().build().expect("build");
        let result = rt.block_on(async move {
            let m = Mutex::new(Vec::new());
            let (tx, mut rx) = mpsc::channel(vals_clone.len().max(1));
            for v in &vals_clone {
                runtime_compat::mpsc_send(&tx, *v).await.expect("send");
            }
            drop(tx);
            while let Some(v) = runtime_compat::mpsc_recv_option(&mut rx).await {
                let mut guard = m.lock().await;
                guard.push(v);
            }
            let guard = m.lock().await;
            guard.clone()
        });
        assert_eq!(result, vals, "runtime+channel+mutex workflow must preserve values");
    }
}

// ────────────────────────────────────────────────────────────────────
// join! macro properties
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// join! preserves both values from concurrent futures.
    #[test]
    fn join_preserves_both_values(a in any::<i64>(), b in any::<i64>()) {
        with_tokio(move || async move {
            let (ra, rb) = runtime_compat::join!(async { a }, async { b });
            assert_eq!(ra, a, "first future value preserved");
            assert_eq!(rb, b, "second future value preserved");
        });
    }

    /// join! preserves ordering of three futures (all values returned).
    #[test]
    fn join_three_preserves_all(a in any::<i32>(), b in any::<i32>(), c in any::<i32>()) {
        with_tokio(move || async move {
            let (ra, rb, rc) = runtime_compat::join!(
                async { a },
                async { b },
                async { c }
            );
            assert_eq!(ra, a, "first value");
            assert_eq!(rb, b, "second value");
            assert_eq!(rc, c, "third value");
        });
    }

    /// join! with channel operations: send on one side, receive on the other.
    #[test]
    fn join_channel_roundtrip(val in any::<u64>()) {
        with_tokio(move || async move {
            let (tx, mut rx) = mpsc::channel(1);
            let ((), recv_result) = runtime_compat::join!(
                async {
                    tx.send(val).await.expect("send");
                },
                async {
                    rx.recv().await.expect("recv")
                }
            );
            assert_eq!(recv_result, val, "channel+join must preserve value");
        });
    }
}

// ────────────────────────────────────────────────────────────────────
// select! macro properties
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// select! returns the value from an immediately-ready branch.
    #[test]
    fn select_immediate_branch_returns_value(val in any::<i64>()) {
        with_tokio(move || async move {
            let result = runtime_compat::select! {
                v = async { val } => v,
                () = runtime_compat::sleep(Duration::from_secs(60)) => -1,
            };
            assert_eq!(result, val, "immediate branch should win");
        });
    }

    /// select! with channel: ready channel wins over sleep.
    #[test]
    fn select_channel_wins_over_sleep(val in any::<u32>()) {
        with_tokio(move || async move {
            let (tx, mut rx) = mpsc::channel(1);
            tx.send(val).await.expect("send");
            let result = runtime_compat::select! {
                maybe = rx.recv() => maybe.unwrap_or(0),
                () = runtime_compat::sleep(Duration::from_secs(60)) => 0,
            };
            assert_eq!(result, val, "channel should win over long sleep");
        });
    }

    /// select! biased always picks the first ready branch.
    #[test]
    fn select_biased_first_wins(a in any::<i32>(), b in any::<i32>()) {
        with_tokio(move || async move {
            let result = runtime_compat::select! {
                biased;
                v = async { a } => v,
                v = async { b } => v,
            };
            assert_eq!(result, a, "biased select must pick first ready branch");
        });
    }
}

// ────────────────────────────────────────────────────────────────────
// task::spawn_blocking (JoinHandle) properties
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// task::spawn_blocking preserves the return value through the JoinHandle.
    #[test]
    fn task_spawn_blocking_preserves_value(val in any::<i64>()) {
        with_tokio(move || async move {
            let handle = runtime_compat::task::spawn_blocking(move || val);
            let result = handle.await.expect("join");
            assert_eq!(result, val, "spawn_blocking must preserve closure return");
        });
    }

    /// task::spawn_blocking with computation: sum of range preserved.
    #[test]
    fn task_spawn_blocking_computation(n in 0u64..1000) {
        let expected: u64 = (0..n).sum();
        with_tokio(move || async move {
            let handle = runtime_compat::task::spawn_blocking(move || {
                (0..n).sum::<u64>()
            });
            let result = handle.await.expect("join");
            assert_eq!(result, expected, "blocking computation must match");
        });
    }
}

// ────────────────────────────────────────────────────────────────────
// task::yield_now properties
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// yield_now does not lose values: interleaved yields preserve counter.
    #[test]
    fn yield_preserves_counter(n in 1usize..50) {
        with_tokio(move || async move {
            let mut counter = 0usize;
            for _ in 0..n {
                counter += 1;
                runtime_compat::task::yield_now().await;
            }
            assert_eq!(counter, n, "yield must not lose increments");
        });
    }
}

// ────────────────────────────────────────────────────────────────────
// Composite multi-primitive properties
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// spawn_blocking + join!: blocking computation and async work run concurrently.
    #[test]
    fn spawn_blocking_with_join(a in 0u64..100, b in 0u64..100) {
        with_tokio(move || async move {
            let blocking_handle = runtime_compat::task::spawn_blocking(move || {
                (0..a).sum::<u64>()
            });
            let (blocking_result, async_result) = runtime_compat::join!(
                async { blocking_handle.await.expect("blocking join") },
                async { (0..b).sum::<u64>() }
            );
            let expected_a: u64 = (0..a).sum();
            let expected_b: u64 = (0..b).sum();
            assert_eq!(blocking_result, expected_a, "blocking arm");
            assert_eq!(async_result, expected_b, "async arm");
        });
    }

    /// select! with channel + timeout: value arrives before timeout.
    #[test]
    fn select_channel_before_timeout(val in any::<u64>()) {
        with_tokio(move || async move {
            let (tx, mut rx) = mpsc::channel(1);
            // Send immediately so channel is ready before timeout.
            tx.send(val).await.expect("send");
            let result = runtime_compat::select! {
                v = rx.recv() => v.unwrap_or(0),
                () = runtime_compat::sleep(Duration::from_secs(60)) => u64::MAX,
            };
            assert_eq!(result, val, "channel should resolve before timeout");
        });
    }

    /// Mutex + spawn: concurrent tasks safely accumulate into shared state.
    #[test]
    fn mutex_concurrent_spawn_accumulation(n in 1usize..20) {
        with_tokio(move || async move {
            let shared = Arc::new(Mutex::new(0u64));
            let mut handles = Vec::new();
            for _ in 0..n {
                let m = shared.clone();
                handles.push(runtime_compat::task::spawn(async move {
                    let mut guard = m.lock().await;
                    *guard += 1;
                }));
            }
            for h in handles {
                h.await.expect("task");
            }
            let final_val = *shared.lock().await;
            assert_eq!(final_val, n as u64, "all spawned tasks must increment");
        });
    }

    /// Channel fan-in: multiple senders, single receiver collects all values.
    #[test]
    fn channel_fan_in_collects_all(vals in proptest::collection::vec(any::<i32>(), 1..20)) {
        with_tokio(move || async move {
            let (tx, mut rx) = mpsc::channel(vals.len() + 1);
            for v in &vals {
                let tx_clone = tx.clone();
                let v = *v;
                runtime_compat::task::spawn(async move {
                    tx_clone.send(v).await.expect("send");
                });
            }
            drop(tx); // Drop original sender so rx completes.
            let mut received = Vec::new();
            while let Some(v) = runtime_compat::mpsc_recv_option(&mut rx).await {
                received.push(v);
            }
            received.sort();
            let mut expected = vals.clone();
            expected.sort();
            assert_eq!(received, expected, "fan-in must collect all values");
        });
    }

    /// Semaphore + spawn: bounded concurrency with N permits.
    #[test]
    fn semaphore_bounds_concurrency(permits in 1usize..5, tasks in 2usize..10) {
        with_tokio(move || async move {
            let sem = Arc::new(Semaphore::new(permits));
            let completed = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let mut handles = Vec::new();
            for _ in 0..tasks {
                let s = sem.clone();
                let c = completed.clone();
                handles.push(runtime_compat::task::spawn(async move {
                    let _permit = s.acquire().await.unwrap();
                    c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    runtime_compat::task::yield_now().await;
                }));
            }
            for h in handles {
                h.await.expect("task");
            }
            let total = completed.load(std::sync::atomic::Ordering::SeqCst);
            assert_eq!(total, tasks as u64, "all tasks must complete");
        });
    }

    /// Watch + select!: latest value observed when combined with timeout.
    #[test]
    fn watch_latest_via_select(vals in proptest::collection::vec(1i32..1000, 1..10)) {
        with_tokio(move || async move {
            let (tx, mut rx) = runtime_compat::watch::channel(0i32);
            for v in &vals {
                tx.send(*v).expect("send");
            }
            // select! should immediately see the latest value.
            let result = runtime_compat::select! {
                _ = rx.changed() => *rx.borrow(),
                () = runtime_compat::sleep(Duration::from_secs(60)) => -1,
            };
            let last = *vals.last().unwrap();
            assert_eq!(result, last, "watch should reflect latest sent value");
        });
    }

    /// spawn_blocking inside select!: blocking work completes before timeout.
    #[test]
    fn select_spawn_blocking_before_timeout(val in any::<i64>()) {
        with_tokio(move || async move {
            let result = runtime_compat::select! {
                v = async {
                    runtime_compat::task::spawn_blocking(move || val)
                        .await
                        .expect("blocking join")
                } => v,
                () = runtime_compat::sleep(Duration::from_secs(60)) => i64::MIN,
            };
            assert_eq!(result, val, "blocking task should complete before timeout");
        });
    }

    /// Notify + spawn: notified task resumes and produces correct value.
    #[test]
    fn notify_resumes_waiting_task(val in any::<u32>()) {
        with_tokio(move || async move {
            let notify = Arc::new(notify::Notify::new());
            let n = notify.clone();
            let handle = runtime_compat::task::spawn(async move {
                n.notified().await;
                val
            });
            // Give the spawned task a chance to park.
            runtime_compat::task::yield_now().await;
            notify.notify_one();
            let result = handle.await.expect("task");
            assert_eq!(result, val, "notified task must produce correct value");
        });
    }
}
